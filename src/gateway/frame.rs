use std::io;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub const HANDSHAKE: u32 = 0;
pub const FRAME: u32 = 1;
pub const CLOSE: u32 = 2;
#[allow(dead_code)]
pub const PING: u32 = 3;
#[allow(dead_code)]
pub const PONG: u32 = 4;

const MAX_PAYLOAD: u32 = 2 * 1024 * 1024; // 2 MB per Discord RPC spec

pub struct Frame {
    pub opcode: u32,
    pub payload: Vec<u8>,
}

pub fn encode(opcode: u32, payload: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8 + payload.len());
    buf.extend_from_slice(&opcode.to_le_bytes());
    buf.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    buf.extend_from_slice(payload);
    buf
}

pub async fn read<R: AsyncReadExt + Unpin>(r: &mut R) -> io::Result<Frame> {
    let mut header = [0u8; 8];
    r.read_exact(&mut header).await?;

    let opcode = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let length = u32::from_le_bytes(header[4..8].try_into().unwrap());

    if length > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame too large: {length} bytes"),
        ));
    }

    let mut payload = vec![0u8; length as usize];
    r.read_exact(&mut payload).await?;

    Ok(Frame { opcode, payload })
}

pub async fn write<W: AsyncWriteExt + Unpin>(w: &mut W, frame: &Frame) -> io::Result<()> {
    w.write_all(&encode(frame.opcode, &frame.payload)).await
}
