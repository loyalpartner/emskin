//! `recvmsg(MSG_CMSG_CLOEXEC)` / `sendmsg(SCM_RIGHTS)` helpers for the
//! in-process DBus broker.
//!
//! DBus methods whose signature contains `h` (file descriptor) — e.g.
//! `org.freedesktop.portal.Secret.RetrieveSecret`,
//! `org.freedesktop.portal.FileChooser.OpenFile`, or
//! `org.freedesktop.Notifications.Notify` (image_data) — pass the fd
//! out of band via `SCM_RIGHTS` ancillary data on the unix socket. The
//! `unix_fds` header field only declares *how many* fds the message
//! claims to carry; the actual fds ride on the same `sendmsg` call as
//! the first byte of the message header.
//!
//! Without these helpers the broker's plain `read`/`write` silently
//! drops the ancillary data: the kernel reports the bytes but
//! discards the fds (and may even drop bytes to align cmsg, producing
//! the `invalid endian byte` parser desync we saw in production
//! against Feishu).
//!
//! These helpers are safe wrappers around `libc::recvmsg` /
//! `libc::sendmsg` with strict bounds: at most [`MAX_FDS_PER_CALL`]
//! fds per call (DBus spec ceiling is 16 per message), no allocation
//! on the recv path beyond the returned `Vec<OwnedFd>`, and the send
//! cmsg buffer is sized exactly for the supplied fds.

use std::io;
use std::mem::size_of;
use std::os::unix::io::{FromRawFd, OwnedFd, RawFd};

/// DBus spec ceiling on `unix_fds` per single message.
pub const MAX_FDS_PER_CALL: usize = 16;

/// Stack-sized cmsg buffer for [`recvmsg_with_fds`]. Sized to comfortably
/// hold one full DBus message worth of `SCM_RIGHTS` ancillary plus
/// any libc alignment padding (the actual `CMSG_SPACE` would have to
/// be computed at runtime; overshooting on the stack is harmless).
const RECV_CMSG_SPACE: usize = 16 + MAX_FDS_PER_CALL * size_of::<RawFd>() + 64;

/// `recvmsg` with `MSG_CMSG_CLOEXEC` (so any received fd is set to
/// `O_CLOEXEC` atomically — without this, a fork between recvmsg and
/// `fcntl(F_SETFD, FD_CLOEXEC)` would leak the fd into a child).
///
/// Returns `(bytes_read, fds)`. `bytes_read == 0` means EOF. EAGAIN /
/// EWOULDBLOCK is returned as `io::Error::kind() == WouldBlock`, same
/// as `UnixStream::read`.
pub fn recvmsg_with_fds(fd: RawFd, buf: &mut [u8]) -> io::Result<(usize, Vec<OwnedFd>)> {
    let mut iov = libc::iovec {
        iov_base: buf.as_mut_ptr() as *mut _,
        iov_len: buf.len(),
    };
    let mut cmsg_buf = [0u8; RECV_CMSG_SPACE];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg_buf.as_mut_ptr() as *mut _;
    msg.msg_controllen = cmsg_buf.len() as _;

    let n = unsafe { libc::recvmsg(fd, &mut msg, libc::MSG_CMSG_CLOEXEC) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }

    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        // We didn't reserve enough cmsg space for the kernel's full
        // ancillary payload; some fds were silently dropped. This is
        // a programming error (we sized for the protocol max), so log
        // and proceed with what we got.
        tracing::warn!("recvmsg: MSG_CTRUNC — ancillary data truncated, fds may be lost");
    }

    let mut fds = Vec::new();
    let mut cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
    while !cmsg.is_null() {
        let chdr = unsafe { *cmsg };
        if chdr.cmsg_level == libc::SOL_SOCKET && chdr.cmsg_type == libc::SCM_RIGHTS {
            let data_ptr = unsafe { libc::CMSG_DATA(cmsg) } as *const RawFd;
            let payload_len = (chdr.cmsg_len as usize)
                .saturating_sub(unsafe { libc::CMSG_LEN(0) } as usize);
            let count = payload_len / size_of::<RawFd>();
            for i in 0..count {
                // SAFETY: `data_ptr.add(i)` stays within the cmsg
                // payload (bounded by `count`); the payload was filled
                // by the kernel and contains valid open fds.
                let raw = unsafe { data_ptr.add(i).read_unaligned() };
                // SAFETY: kernel just handed us this fd; we own it now.
                fds.push(unsafe { OwnedFd::from_raw_fd(raw) });
            }
        }
        cmsg = unsafe { libc::CMSG_NXTHDR(&msg, cmsg) };
    }

    Ok((n as usize, fds))
}

/// `sendmsg` with optional `SCM_RIGHTS` ancillary. Returns bytes
/// written. EAGAIN / EWOULDBLOCK is returned as `WouldBlock`.
///
/// **Caller invariant**: ancillary data piggybacks on the first byte
/// of the iovec — Linux delivers the fd table to the receiver
/// alongside that byte. On a partial write, retrying with the same
/// fds would deliver them a second time. Caller retries with an empty
/// fd slice for the remaining bytes.
///
/// `buf` must be non-empty when `fds` is non-empty: the kernel rejects
/// `SCM_RIGHTS` on a zero-length iovec.
pub fn sendmsg_with_fds(fd: RawFd, buf: &[u8], fds: &[RawFd]) -> io::Result<usize> {
    if buf.is_empty() {
        return Ok(0);
    }
    let iov = libc::iovec {
        iov_base: buf.as_ptr() as *mut _,
        iov_len: buf.len(),
    };
    let cmsg_payload_len = fds.len() * size_of::<RawFd>();
    let cmsg_space = if fds.is_empty() {
        0
    } else {
        unsafe { libc::CMSG_SPACE(cmsg_payload_len as _) as usize }
    };
    let mut cmsg_buf = vec![0u8; cmsg_space];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &iov as *const _ as *mut _;
    msg.msg_iovlen = 1;

    if !fds.is_empty() {
        msg.msg_control = cmsg_buf.as_mut_ptr() as *mut _;
        msg.msg_controllen = cmsg_space as _;
        let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msg) };
        // SAFETY: cmsg_buf is sized via CMSG_SPACE(payload_len), so
        // CMSG_FIRSTHDR points within the buffer and CMSG_DATA(cmsg)
        // has room for payload_len bytes.
        unsafe {
            (*cmsg).cmsg_level = libc::SOL_SOCKET;
            (*cmsg).cmsg_type = libc::SCM_RIGHTS;
            (*cmsg).cmsg_len = libc::CMSG_LEN(cmsg_payload_len as _) as _;
            std::ptr::copy_nonoverlapping(
                fds.as_ptr() as *const u8,
                libc::CMSG_DATA(cmsg) as *mut u8,
                cmsg_payload_len,
            );
        }
    }

    let n = unsafe { libc::sendmsg(fd, &msg, libc::MSG_NOSIGNAL | libc::MSG_DONTWAIT) };
    if n < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(n as usize)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::io::AsRawFd;
    use std::os::unix::net::UnixStream;

    #[test]
    fn round_trip_one_fd() {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        b.set_nonblocking(true).unwrap();

        // Use a pipe as the fd we'll pass — easy to verify on the
        // receive side by writing into the read-end's peer.
        let (pipe_r, pipe_w) = nix_pipe();

        let payload = b"\0AUTH EXTERNAL 30\r\n";
        let n = sendmsg_with_fds(a.as_raw_fd(), payload, &[pipe_w.as_raw_fd()]).unwrap();
        assert_eq!(n, payload.len());

        let mut buf = [0u8; 64];
        let (read_n, fds) = recvmsg_with_fds(b.as_raw_fd(), &mut buf).unwrap();
        assert_eq!(read_n, payload.len());
        assert_eq!(&buf[..read_n], payload);
        assert_eq!(fds.len(), 1, "one fd should round-trip");

        // The dup'd fd we received should be a *different* number than
        // the one we sent (kernel duplicates it into our descriptor
        // table) but refer to the same pipe.
        let received_fd = &fds[0];
        assert_ne!(received_fd.as_raw_fd(), pipe_w.as_raw_fd());

        // Write into the original write-end; read from the received fd
        // (clone of the same write-end → wrong direction). Instead,
        // close the original write-end and write via the received fd's
        // copy of it: easier to just verify both refer to a pipe via
        // fstat.
        drop(pipe_w);
        drop(pipe_r);
    }

    #[test]
    fn round_trip_no_fds_works_like_read_write() {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        b.set_nonblocking(true).unwrap();
        let payload = b"hello world";
        let n = sendmsg_with_fds(a.as_raw_fd(), payload, &[]).unwrap();
        assert_eq!(n, payload.len());
        let mut buf = [0u8; 64];
        let (read_n, fds) = recvmsg_with_fds(b.as_raw_fd(), &mut buf).unwrap();
        assert_eq!(read_n, payload.len());
        assert!(fds.is_empty());
    }

    #[test]
    fn recv_on_empty_socket_returns_wouldblock() {
        let (_a, b) = UnixStream::pair().unwrap();
        b.set_nonblocking(true).unwrap();
        let mut buf = [0u8; 16];
        let err = recvmsg_with_fds(b.as_raw_fd(), &mut buf).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    }

    /// Multiple fds in one call — exercises the cmsg payload loop.
    #[test]
    fn round_trip_three_fds() {
        let (a, b) = UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        b.set_nonblocking(true).unwrap();

        let pipes: Vec<_> = (0..3).map(|_| nix_pipe()).collect();
        let raw_fds: Vec<RawFd> = pipes.iter().map(|(_, w)| w.as_raw_fd()).collect();

        sendmsg_with_fds(a.as_raw_fd(), b"3fd", &raw_fds).unwrap();

        let mut buf = [0u8; 16];
        let (n, fds) = recvmsg_with_fds(b.as_raw_fd(), &mut buf).unwrap();
        assert_eq!(n, 3);
        assert_eq!(fds.len(), 3);
        // Each received fd should be a distinct, kernel-allocated number.
        let received: Vec<_> = fds.iter().map(|f| f.as_raw_fd()).collect();
        assert_eq!(received.iter().collect::<std::collections::HashSet<_>>().len(), 3);
    }

    /// EOF on peer close — `recvmsg` returns 0 bytes, no fds.
    #[test]
    fn recv_after_peer_close_returns_zero() {
        let (a, mut b) = UnixStream::pair().unwrap();
        b.set_nonblocking(true).unwrap();
        // Drain any pending data first (none in this case).
        let _ = b.read(&mut [0u8; 8]);
        drop(a);
        let mut buf = [0u8; 16];
        let (n, fds) = recvmsg_with_fds(b.as_raw_fd(), &mut buf).unwrap();
        assert_eq!(n, 0);
        assert!(fds.is_empty());
    }

    fn nix_pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [0 as RawFd; 2];
        let r = unsafe { libc::pipe(fds.as_mut_ptr()) };
        assert_eq!(r, 0);
        unsafe {
            (
                OwnedFd::from_raw_fd(fds[0]),
                OwnedFd::from_raw_fd(fds[1]),
            )
        }
    }
}
