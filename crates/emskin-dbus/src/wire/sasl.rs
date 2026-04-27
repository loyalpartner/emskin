//! SASL handshake scanner for the DBus session bus.
//!
//! The proxy is a transparent byte forwarder during the auth phase: it does
//! not speak SASL itself. This scanner watches the **client → bus** direction
//! for `BEGIN\r\n` so the broker knows when to switch from raw byte forwarding
//! to DBus message parsing.
//!
//! The DBus spec requires that the first byte a client writes on a newly
//! opened connection is NUL (it carries the `SCM_CREDENTIALS` marker on unix
//! sockets). Every subsequent auth line is `\r\n`-terminated ASCII — see
//! <https://dbus.freedesktop.org/doc/dbus-specification.html#auth-protocol>.
//!
//! Reference: `flatpak-proxy.c:find_auth_end` (xdg-dbus-proxy upstream).

use std::{error, fmt};

/// dbus-daemon aborts auth if a single line exceeds 16 KiB; mirror that here.
pub const MAX_AUTH_BUFFER: usize = 16 * 1024;

const SENTINEL: &[u8] = b"\r\n";
const BEGIN: &[u8] = b"BEGIN";

/// Reasons the SASL stream is not parseable and must be aborted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaslError {
    /// First byte of the client stream was not the required NUL marker.
    MissingNulPrefix,
    /// An auth line contained a byte outside `0x20..=0x7e`, or didn't start
    /// with `A..=Z`. dbus-daemon permits any ASCII, but every real client
    /// command is uppercase; rejecting the rest shrinks the attack surface
    /// exactly the way xdg-dbus-proxy does.
    InvalidAuthLine,
    /// Accumulated more than [`MAX_AUTH_BUFFER`] bytes without a newline.
    AuthLineTooLong,
}

impl fmt::Display for SaslError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingNulPrefix => f.write_str("SASL: first byte must be NUL"),
            Self::InvalidAuthLine => f.write_str("SASL: invalid auth line"),
            Self::AuthLineTooLong => f.write_str("SASL: auth line exceeded 16 KiB"),
        }
    }
}

impl error::Error for SaslError {}

/// Scan a client→bus byte buffer for the end of the SASL handshake.
///
/// `buf` is the full stream received from the client since connection open,
/// starting with the required NUL credential byte.
///
/// Returns:
/// - `Ok(Some(end))` — `BEGIN\r\n` was found; `buf[..end]` is the complete
///   auth handshake (forward it verbatim to the upstream bus), and
///   `buf[end..]` is the first chunk of the DBus message stream.
/// - `Ok(None)` — the buffer is still incomplete; feed more bytes and
///   re-scan from the beginning. (The scanner is stateless — the caller owns
///   the accumulator.)
/// - `Err(_)` — the stream is malformed; close the connection.
pub fn find_begin_end(buf: &[u8]) -> Result<Option<usize>, SaslError> {
    if buf.is_empty() {
        return Ok(None);
    }
    if buf[0] != 0 {
        return Err(SaslError::MissingNulPrefix);
    }

    // Everything after the NUL is line-oriented.
    let lines = &buf[1..];
    let mut offset = 0usize;
    while let Some(rel) = find_sentinel(&lines[offset..]) {
        let line = &lines[offset..offset + rel];
        validate_line(line)?;
        let after_line = offset + rel + SENTINEL.len();
        if is_begin_line(line) {
            // +1 for the leading NUL at buf[0].
            return Ok(Some(1 + after_line));
        }
        offset = after_line;
    }

    // No `BEGIN` yet. Guard against run-away input.
    if buf.len() > MAX_AUTH_BUFFER {
        return Err(SaslError::AuthLineTooLong);
    }
    Ok(None)
}

fn find_sentinel(s: &[u8]) -> Option<usize> {
    s.windows(SENTINEL.len()).position(|w| w == SENTINEL)
}

fn validate_line(line: &[u8]) -> Result<(), SaslError> {
    if line.is_empty() {
        return Err(SaslError::InvalidAuthLine);
    }
    if !line[0].is_ascii_uppercase() {
        return Err(SaslError::InvalidAuthLine);
    }
    for &b in line {
        if !(0x20..=0x7e).contains(&b) {
            return Err(SaslError::InvalidAuthLine);
        }
    }
    Ok(())
}

fn is_begin_line(line: &[u8]) -> bool {
    if line.len() < BEGIN.len() || &line[..BEGIN.len()] != BEGIN {
        return false;
    }
    // dbus-daemon treats `BEGIN` followed by whitespace (or EOL) as terminator.
    line.len() == BEGIN.len() || matches!(line[BEGIN.len()], b' ' | b'\t')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffer_returns_none() {
        assert_eq!(find_begin_end(b""), Ok(None));
    }

    #[test]
    fn nul_only_returns_none() {
        assert_eq!(find_begin_end(b"\0"), Ok(None));
    }

    #[test]
    fn missing_nul_errors() {
        assert_eq!(
            find_begin_end(b"AUTH EXTERNAL 30\r\n"),
            Err(SaslError::MissingNulPrefix)
        );
    }

    #[test]
    fn typical_handshake_returns_end_of_begin_line() {
        let stream: &[u8] = b"\0AUTH EXTERNAL 30\r\nNEGOTIATE_UNIX_FD\r\nBEGIN\r\n";
        let end = find_begin_end(stream).unwrap().unwrap();
        assert_eq!(end, stream.len());
        assert_eq!(&stream[..end], stream);
    }

    #[test]
    fn bytes_after_begin_are_outside_end() {
        let handshake: &[u8] = b"\0AUTH EXTERNAL 30\r\nBEGIN\r\n";
        let mut stream = Vec::from(handshake);
        // Synthetic start of a DBus message (fixed header prefix).
        stream.extend_from_slice(&[b'l', 1, 0, 1, 0, 0, 0, 0]);

        let end = find_begin_end(&stream).unwrap().unwrap();
        assert_eq!(end, handshake.len());
        assert_eq!(&stream[end..], &[b'l', 1, 0, 1, 0, 0, 0, 0]);
    }

    #[test]
    fn partial_line_returns_none() {
        let stream: &[u8] = b"\0AUTH EXTERNAL 30\r\nBEGI";
        assert_eq!(find_begin_end(stream), Ok(None));
    }

    #[test]
    fn pre_begin_lines_only_returns_none() {
        let stream: &[u8] = b"\0AUTH EXTERNAL 30\r\nNEGOTIATE_UNIX_FD\r\n";
        assert_eq!(find_begin_end(stream), Ok(None));
    }

    #[test]
    fn control_char_in_line_is_rejected() {
        let stream: &[u8] = b"\0AU\x01TH EXTERNAL\r\n";
        assert_eq!(find_begin_end(stream), Err(SaslError::InvalidAuthLine));
    }

    #[test]
    fn high_bit_char_in_line_is_rejected() {
        let stream: &[u8] = b"\0AUTH EXT\xa0ERNAL\r\n";
        assert_eq!(find_begin_end(stream), Err(SaslError::InvalidAuthLine));
    }

    #[test]
    fn lowercase_leading_char_is_rejected() {
        let stream: &[u8] = b"\0auth EXTERNAL\r\n";
        assert_eq!(find_begin_end(stream), Err(SaslError::InvalidAuthLine));
    }

    #[test]
    fn begin_with_trailing_space_terminates() {
        // dbus-daemon splits commands on space; validate_line rejects tabs
        // and other control characters, so `BEGIN\t` can't occur in practice
        // — only the space form is exercised here.
        let stream: &[u8] = b"\0AUTH EXTERNAL\r\nBEGIN \r\n";
        let end = find_begin_end(stream).unwrap().unwrap();
        assert_eq!(end, stream.len());
    }

    #[test]
    fn begin_prefix_without_terminator_is_not_begin() {
        // `BEGINNER` must not be accepted as BEGIN (prefix match).
        let stream: &[u8] = b"\0BEGINNER\r\n";
        assert_eq!(find_begin_end(stream), Ok(None));
    }

    #[test]
    fn over_16_kib_without_newline_errors() {
        let mut stream = Vec::with_capacity(MAX_AUTH_BUFFER + 2);
        stream.push(0);
        stream.resize(MAX_AUTH_BUFFER + 2, b'A');
        assert_eq!(find_begin_end(&stream), Err(SaslError::AuthLineTooLong));
    }
}
