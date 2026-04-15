use socket2::{Domain, SockAddr, Socket, Type};
use std::io;
use std::path::PathBuf;
use tokio::sync::mpsc;

use super::frame::{self, Frame};

/// Resolve the Unix socket path for a given slot.
fn resolve_socket_path(slot: u8) -> Option<PathBuf> {
    crate::config::resolve_socket_dir().map(|dir| dir.join(format!("discord-ipc-{slot}")))
}

/// Blocking connect to Discord Unix socket for the given slot.
fn connect_blocking(slot: u8) -> io::Result<Socket> {
    let path =
        resolve_socket_path(slot).ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no IPC path"))?;
    let sock = Socket::new(Domain::UNIX, Type::STREAM, None)?;
    sock.connect(&SockAddr::unix(&path)?)?;
    Ok(sock)
}

/// Connected Discord session wrapping a blocking socket2::Socket.
/// All I/O is dispatched to spawn_blocking to avoid blocking the async runtime.
pub struct Connection {
    sock: Socket,
    pub client_id: String,
    pub cached_ready: Option<Frame>,
}

impl Connection {
    /// Connect to Discord and perform HANDSHAKE with the given client_id.
    pub async fn open(slot: u8, client_id: &str) -> io::Result<Self> {
        let cid = client_id.to_owned();
        let sock = tokio::task::spawn_blocking(move || connect_blocking(slot))
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))??;

        let mut conn = Self {
            sock,
            client_id: cid,
            cached_ready: None,
        };

        // Send HANDSHAKE
        let handshake = format!(r#"{{"v":1,"client_id":"{}"}}"#, conn.client_id);
        conn.write_frame(&Frame {
            opcode: frame::HANDSHAKE,
            payload: handshake.into_bytes(),
        })
        .await?;

        // Read READY / ERROR response (single read, no reader task yet)
        let response = conn.read_frame_once().await?;
        if response.opcode == frame::CLOSE {
            let msg = String::from_utf8_lossy(&response.payload);
            return Err(io::Error::new(io::ErrorKind::ConnectionRefused, msg.into_owned()));
        }
        conn.cached_ready = Some(Frame {
            opcode: response.opcode,
            payload: response.payload,
        });

        Ok(conn)
    }

    /// Spawn a dedicated reader task that continuously reads frames from Discord.
    /// Returns a channel receiver. The task exits when the socket errors or the
    /// receiver is dropped AND Discord sends the next frame.
    /// IMPORTANT: Only one reader should be active per connection at a time.
    pub fn start_reader(&self) -> io::Result<mpsc::Receiver<io::Result<Frame>>> {
        let sock = self.sock.try_clone()?;
        let (tx, rx) = mpsc::channel(32);
        tokio::task::spawn_blocking(move || {
            loop {
                match read_frame_blocking(&sock) {
                    Ok(f) => {
                        if tx.blocking_send(Ok(f)).is_err() {
                            break; // receiver dropped
                        }
                    }
                    Err(e) => {
                        let _ = tx.blocking_send(Err(e));
                        break;
                    }
                }
            }
        });
        Ok(rx)
    }

    /// Read one frame (used only during HANDSHAKE, before reader task starts).
    async fn read_frame_once(&self) -> io::Result<Frame> {
        let sock = self.sock.try_clone()?;
        tokio::task::spawn_blocking(move || read_frame_blocking(&sock))
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
    }

    /// Write one frame to the Discord socket.
    pub async fn write_frame(&self, f: &Frame) -> io::Result<()> {
        let sock = self.sock.try_clone()?;
        let data = frame::encode(f.opcode, &f.payload);
        tokio::task::spawn_blocking(move || send_all(&sock, &data))
            .await
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?
    }

    /// Send SET_ACTIVITY(null) to clear the current activity.
    pub async fn send_null_activity(&self) -> io::Result<()> {
        let payload =
            br#"{"cmd":"SET_ACTIVITY","args":{"pid":0,"activity":null},"nonce":"xbridge-clear"}"#;
        self.write_frame(&Frame {
            opcode: frame::FRAME,
            payload: payload.to_vec(),
        })
        .await
    }

    /// Send SET_ACTIVITY with discovery info.
    pub async fn send_discovery_activity(&self, app_name: &str, pid: u32) -> io::Result<()> {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let payload = serde_json::json!({
            "cmd": "SET_ACTIVITY",
            "args": {
                "pid": pid,
                "activity": {
                    "details": format!("Playing {app_name}"),
                    "timestamps": { "start": ts }
                }
            },
            "nonce": "xbridge-discovery"
        });
        let bytes = serde_json::to_vec(&payload)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        self.write_frame(&Frame {
            opcode: frame::FRAME,
            payload: bytes,
        })
        .await
    }

    /// Best-effort: send SET_ACTIVITY(null) then shutdown the socket.
    /// Shutting down the socket also kills any active reader task.
    pub async fn clear_and_close(&self) {
        let _ = self.send_null_activity().await;
        let _ = self.sock.shutdown(std::net::Shutdown::Both);
    }
}

// --- blocking helpers ---

fn send_all(sock: &Socket, mut data: &[u8]) -> io::Result<()> {
    while !data.is_empty() {
        let n = sock.send(data)?;
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::WriteZero));
        }
        data = &data[n..];
    }
    Ok(())
}

fn recv_exact(sock: &Socket, buf: &mut [u8]) -> io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        let mut tmp = vec![std::mem::MaybeUninit::uninit(); buf.len() - filled];
        let n = sock.recv(&mut tmp)?;
        if n == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        // SAFETY: recv filled exactly n bytes
        unsafe {
            std::ptr::copy_nonoverlapping(tmp.as_ptr() as *const u8, buf[filled..].as_mut_ptr(), n);
        }
        filled += n;
    }
    Ok(())
}

fn read_frame_blocking(sock: &Socket) -> io::Result<Frame> {
    let mut header = [0u8; 8];
    recv_exact(sock, &mut header)?;

    let opcode = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let length = u32::from_le_bytes(header[4..8].try_into().unwrap());

    if length > 2 * 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame too large: {length}"),
        ));
    }

    let mut payload = vec![0u8; length as usize];
    recv_exact(sock, &mut payload)?;

    Ok(Frame { opcode, payload })
}
