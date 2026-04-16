use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;

use super::messages::OutgoingMessage;

/// Maximum allowed IPC message payload size (1 MiB).
const MAX_MSG_SIZE: usize = 1024 * 1024;

/// A single active IPC connection (one Emacs client).
pub struct IpcConn {
    pub(super) stream: UnixStream,
    /// Incomplete incoming bytes waiting to form a full message.
    read_buf: Vec<u8>,
    /// Serialized bytes queued for writing.
    write_buf: VecDeque<u8>,
}

impl IpcConn {
    pub fn new(stream: UnixStream) -> io::Result<Self> {
        stream.set_nonblocking(true)?;
        Ok(Self {
            stream,
            read_buf: Vec::new(),
            write_buf: VecDeque::new(),
        })
    }

    /// Drain available bytes from the stream into `read_buf`.
    /// Returns `true` if the peer closed the connection.
    pub fn fill_read_buf(&mut self) -> io::Result<bool> {
        let mut tmp = [0u8; 4096];
        loop {
            match self.stream.read(&mut tmp) {
                Ok(0) => return Ok(true), // EOF — peer closed
                Ok(n) => self.read_buf.extend_from_slice(&tmp[..n]),
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
                Err(e) => return Err(e),
            }
        }
    }

    /// Attempt to decode and return the next complete message, if available.
    /// Returns `Err` if the framed length exceeds `MAX_MSG_SIZE`.
    pub fn try_recv(&mut self) -> io::Result<Option<Vec<u8>>> {
        if self.read_buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes([
            self.read_buf[0],
            self.read_buf[1],
            self.read_buf[2],
            self.read_buf[3],
        ]) as usize;
        if len > MAX_MSG_SIZE {
            self.read_buf.clear();
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("IPC message size {len} exceeds maximum {MAX_MSG_SIZE}"),
            ));
        }
        if self.read_buf.len() < 4 + len {
            return Ok(None);
        }
        let payload = self.read_buf[4..4 + len].to_vec();
        self.read_buf.drain(..4 + len);
        Ok(Some(payload))
    }

    /// Enqueue a message for sending (4-byte LE length prefix + JSON).
    pub fn enqueue(&mut self, msg: &OutgoingMessage) {
        match serde_json::to_vec(msg) {
            Ok(json) => {
                let len = match u32::try_from(json.len()) {
                    Ok(n) => n,
                    Err(_) => {
                        tracing::error!("IPC message too large to frame ({} bytes)", json.len());
                        return;
                    }
                };
                self.write_buf.extend(len.to_le_bytes());
                self.write_buf.extend(json);
            }
            Err(e) => tracing::error!("IPC serialize error: {e}"),
        }
    }

    /// Flush as many bytes as possible from `write_buf` without blocking.
    /// Returns `true` if there is still data remaining to write.
    pub fn try_flush(&mut self) -> io::Result<bool> {
        while !self.write_buf.is_empty() {
            // Collect contiguous bytes for a single write call.
            let (front, back) = self.write_buf.as_slices();
            let slice = if !front.is_empty() { front } else { back };
            match self.stream.write(slice) {
                Ok(0) => return Ok(!self.write_buf.is_empty()),
                Ok(n) => {
                    self.write_buf.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    return Ok(true);
                }
                Err(e) => return Err(e),
            }
        }
        Ok(false)
    }

    #[allow(dead_code)]
    pub fn has_pending_writes(&self) -> bool {
        !self.write_buf.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixStream;

    fn make_pair() -> (IpcConn, UnixStream) {
        let (a, b) = UnixStream::pair().unwrap();
        (IpcConn::new(a).unwrap(), b)
    }

    /// Write a properly framed message to the raw stream.
    fn write_framed(stream: &mut UnixStream, payload: &[u8]) {
        let len = payload.len() as u32;
        stream.write_all(&len.to_le_bytes()).unwrap();
        stream.write_all(payload).unwrap();
    }

    #[test]
    fn try_recv_returns_none_on_empty_buffer() {
        let (mut conn, _peer) = make_pair();
        assert!(conn.try_recv().unwrap().is_none());
    }

    #[test]
    fn try_recv_returns_none_on_incomplete_header() {
        let (mut conn, mut peer) = make_pair();
        // Write only 2 bytes (header needs 4).
        peer.write_all(&[0x05, 0x00]).unwrap();
        conn.fill_read_buf().ok();
        assert!(conn.try_recv().unwrap().is_none());
    }

    #[test]
    fn try_recv_returns_none_on_incomplete_payload() {
        let (mut conn, mut peer) = make_pair();
        // Header says 10 bytes, but only write 5.
        peer.write_all(&10u32.to_le_bytes()).unwrap();
        peer.write_all(b"hello").unwrap();
        conn.fill_read_buf().ok();
        assert!(conn.try_recv().unwrap().is_none());
    }

    #[test]
    fn try_recv_decodes_single_message() {
        let (mut conn, mut peer) = make_pair();
        let payload = b"hello world";
        write_framed(&mut peer, payload);
        conn.fill_read_buf().ok();
        let msg = conn.try_recv().unwrap().unwrap();
        assert_eq!(msg, payload);
    }

    #[test]
    fn try_recv_handles_multiple_messages_in_one_read() {
        let (mut conn, mut peer) = make_pair();
        write_framed(&mut peer, b"msg1");
        write_framed(&mut peer, b"msg2");
        write_framed(&mut peer, b"msg3");
        conn.fill_read_buf().ok();

        assert_eq!(conn.try_recv().unwrap().unwrap(), b"msg1");
        assert_eq!(conn.try_recv().unwrap().unwrap(), b"msg2");
        assert_eq!(conn.try_recv().unwrap().unwrap(), b"msg3");
        assert!(conn.try_recv().unwrap().is_none());
    }

    #[test]
    fn try_recv_handles_empty_payload() {
        let (mut conn, mut peer) = make_pair();
        write_framed(&mut peer, b"");
        conn.fill_read_buf().ok();
        let msg = conn.try_recv().unwrap().unwrap();
        assert!(msg.is_empty());
    }

    #[test]
    fn try_recv_rejects_oversized_message() {
        let (mut conn, mut peer) = make_pair();
        // Write a header claiming 2 MiB (exceeds MAX_MSG_SIZE of 1 MiB).
        let huge_len = (2 * 1024 * 1024u32).to_le_bytes();
        peer.write_all(&huge_len).unwrap();
        conn.fill_read_buf().ok();
        let result = conn.try_recv();
        assert!(result.is_err());
    }

    #[test]
    fn enqueue_and_flush_roundtrip() {
        let (mut conn, mut peer) = make_pair();
        peer.set_nonblocking(true).unwrap();

        let msg = OutgoingMessage::Connected { version: "0.1.0" };
        conn.enqueue(&msg);
        assert!(conn.has_pending_writes());

        conn.try_flush().unwrap();
        assert!(!conn.has_pending_writes());

        // Read the framed message from the peer side.
        peer.set_nonblocking(false).unwrap();
        let mut header = [0u8; 4];
        peer.read_exact(&mut header).unwrap();
        let len = u32::from_le_bytes(header) as usize;
        let mut payload = vec![0u8; len];
        peer.read_exact(&mut payload).unwrap();

        let json: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(json["type"], "connected");
        assert_eq!(json["version"], "0.1.0");
    }

    #[test]
    fn fill_read_buf_detects_eof() {
        let (mut conn, peer) = make_pair();
        drop(peer); // Close the peer end.
        let eof = conn.fill_read_buf().unwrap();
        assert!(eof);
    }
}
