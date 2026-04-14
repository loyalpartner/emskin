mod connection;
pub mod dispatch;
pub mod messages;

use std::os::unix::net::UnixListener;

use connection::IpcConn;
pub use messages::{IncomingMessage, OutgoingMessage, SkeletonRect};

/// IPC server: listens for a single Emacs connection and exchanges JSON messages.
pub struct IpcServer {
    pub socket_path: std::path::PathBuf,
    listener: UnixListener,
    connection: Option<IpcConn>,
    /// Messages queued before Emacs connects.
    pending: Vec<OutgoingMessage>,
}

impl IpcServer {
    /// Create a listening socket at `path` (non-blocking).
    pub fn bind(path: impl Into<std::path::PathBuf>) -> std::io::Result<Self> {
        let path = path.into();
        // Remove stale socket file if it exists.
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        tracing::info!("IPC listening on {}", path.display());
        Ok(Self {
            socket_path: path,
            listener,
            connection: None,
            pending: Vec::new(),
        })
    }

    /// Accept a pending connection if available.
    /// Returns `true` if a new client connected.
    pub fn accept(&mut self) -> bool {
        match self.listener.accept() {
            Ok((stream, _)) => {
                match IpcConn::new(stream) {
                    Ok(mut conn) => {
                        tracing::info!("Emacs IPC connected");
                        // Send handshake + any buffered messages.
                        conn.enqueue(&OutgoingMessage::Connected { version: "0.1" });
                        for msg in self.pending.drain(..) {
                            conn.enqueue(&msg);
                        }
                        let _ = conn.try_flush();
                        self.connection = Some(conn);
                        true
                    }
                    Err(e) => {
                        tracing::error!("IPC accept error: {e}");
                        false
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(e) => {
                tracing::error!("IPC listener error: {e}");
                false
            }
        }
    }

    /// Send a message. If no client is connected, the message is buffered.
    pub fn send(&mut self, msg: OutgoingMessage) {
        if let Some(conn) = &mut self.connection {
            conn.enqueue(&msg);
            if let Err(e) = conn.try_flush() {
                tracing::warn!("IPC write error: {e}");
            }
        } else {
            self.pending.push(msg);
        }
    }

    /// Read and dispatch all complete incoming messages.
    /// Returns decoded messages (caller decides how to handle them).
    /// Returns `None` on connection close/error (caller should handle disconnect).
    pub fn recv_all(&mut self) -> Option<Vec<IncomingMessage>> {
        let conn = self.connection.as_mut()?;
        match conn.fill_read_buf() {
            Err(e) => {
                tracing::warn!("IPC read error: {e}");
                self.connection = None;
                return None;
            }
            Ok(true) => {
                tracing::info!("Emacs IPC disconnected");
                self.connection = None;
                return None;
            }
            Ok(false) => {}
        }

        let mut msgs = Vec::new();
        loop {
            match conn.try_recv() {
                Ok(Some(payload)) => match serde_json::from_slice::<IncomingMessage>(&payload) {
                    Ok(msg) => msgs.push(msg),
                    Err(e) => {
                        tracing::warn!(
                            "IPC parse error: {e} — payload: {}",
                            String::from_utf8_lossy(&payload)
                        );
                    }
                },
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!("IPC protocol error: {e}");
                    self.connection = None;
                    return None;
                }
            }
        }
        Some(msgs)
    }

    /// Flush pending writes (call after inserting WRITE interest in calloop).
    pub fn flush(&mut self) {
        if let Some(conn) = &mut self.connection {
            if let Err(e) = conn.try_flush() {
                tracing::warn!("IPC flush error: {e}");
                self.connection = None;
            }
        }
    }

    pub fn is_connected(&self) -> bool {
        self.connection.is_some()
    }

    /// Raw fd of the listener socket (for calloop registration).
    pub fn listener_fd(&self) -> std::os::unix::io::RawFd {
        use std::os::unix::io::AsRawFd;
        self.listener.as_raw_fd()
    }

    /// Raw fd of the active connection, if any.
    pub fn connection_fd(&self) -> Option<std::os::unix::io::RawFd> {
        use std::os::unix::io::AsRawFd;
        self.connection.as_ref().map(|c| c.stream.as_raw_fd())
    }
}

impl Drop for IpcServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}
