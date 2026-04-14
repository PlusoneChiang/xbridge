use crate::shared::{HandoffSignal, SlotRegistry};
use socket2::{Domain, SockAddr, Socket, Type};
use std::mem::MaybeUninit;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::ServerOptions;
use tokio::sync::mpsc;

mod path_resolver;
pub use path_resolver::resolve;

pub const CLOSE_FRAME: &[u8] =
    b"\x02\x00\x00\x00\x28\x00\x00\x00{\"code\":1000,\"message\":\"Normal Closure\"}";

pub fn make_frame(opcode: u32, payload: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(8 + payload.len());
    v.extend_from_slice(&opcode.to_le_bytes());
    v.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    v.extend_from_slice(payload);
    v
}

/// Send all bytes to a socket2::Socket.
pub fn sock_send_all(sock: &Socket, mut data: &[u8]) -> std::io::Result<()> {
    while !data.is_empty() {
        let n = sock.send(data)?;
        if n == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
        }
        data = &data[n..];
    }
    Ok(())
}

pub async fn run(
    slots: SlotRegistry,
    handoff_tx: mpsc::Sender<HandoffSignal>,
    reclaim_tx: mpsc::Sender<u8>,
) {
    for slot in 0u8..10 {
        let slots = slots.clone();
        let tx = handoff_tx.clone();
        let reclaim = reclaim_tx.clone();
        tokio::spawn(pipe_slot_loop(slot, slots, tx, reclaim));
    }
    std::future::pending::<()>().await
}

async fn pipe_slot_loop(
    slot: u8,
    slots: SlotRegistry,
    handoff_tx: mpsc::Sender<HandoffSignal>,
    reclaim_tx: mpsc::Sender<u8>,
) {
    let pipe_name = format!(r"\\.\pipe\discord-ipc-{slot}");
    loop {
        let server = match ServerOptions::new().first_pipe_instance(false).create(&pipe_name) {
            Ok(s) => s,
            Err(e) => {
                crate::log!("[bridge] slot {slot} create error: {e}");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };
        if let Err(e) = server.connect().await {
            crate::log!("[bridge] slot {slot} accept error: {e}");
            continue;
        }
        if let Err(e) = handle_connection(slot, server, &slots, &handoff_tx).await {
            crate::log!("[bridge] slot {slot} session error: {e}");
        }
        // Notify discovery that the game's RPC session ended so it can resume
        let _ = reclaim_tx.send(slot).await;
    }
}

async fn handle_connection(
    slot: u8,
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    slots: &SlotRegistry,
    handoff_tx: &mpsc::Sender<HandoffSignal>,
) -> anyhow::Result<()> {
    let (mut pipe_r, mut pipe_w) = tokio::io::split(pipe);

    // Read HANDSHAKE (opcode 0) from the game
    let mut header = [0u8; 8];
    pipe_r.read_exact(&mut header).await?;
    let opcode = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let length = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let mut payload = vec![0u8; length as usize];
    pipe_r.read_exact(&mut payload).await?;

    if opcode != 0 {
        return Ok(());
    }

    // Always signal discovery to release slot N before we take it over.
    crate::log!("[handoff] slot {slot} requesting discovery to release");
    let (ack_tx, ack_rx) = tokio::sync::oneshot::channel();
    let _ = handoff_tx.send(HandoffSignal { slot, ack: ack_tx }).await;
    match tokio::time::timeout(std::time::Duration::from_secs(1), ack_rx).await {
        Ok(_) => crate::log!("[handoff] slot {slot} ready, opening socket"),
        Err(_) => crate::log!("[handoff] slot {slot} timed out waiting for discovery"),
    }

    slots.mark(slot);

    let unix_path = path_resolver::resolve(slot)
        .ok_or_else(|| anyhow::anyhow!("no IPC path configured"))?;
    crate::log!("[bridge] slot {slot} connecting to {}", unix_path.display());
    let unix = tokio::task::spawn_blocking(move || -> anyhow::Result<Socket> {
        let sock = Socket::new(Domain::UNIX, Type::STREAM, None)?;
        sock.connect(&SockAddr::unix(&unix_path)?)?;
        Ok(sock)
    })
    .await
    .map_err(|e| anyhow::anyhow!("spawn error: {e}"))
    .and_then(|r| r.map_err(|e| { crate::log!("[bridge] slot {slot} socket connect failed: {e}"); e }))?;

    // Forward the HANDSHAKE frame to Discord
    let handshake_frame = make_frame(opcode, &payload);
    let handshake_sock = unix.try_clone()?;
    tokio::task::spawn_blocking(move || sock_send_all(&handshake_sock, &handshake_frame))
        .await??;

    // --- Bidirectional copy ---
    let (p2u_tx, mut p2u_rx) = mpsc::channel::<Vec<u8>>(16);
    let (u2p_tx, mut u2p_rx) = mpsc::channel::<Vec<u8>>(16);

    // t1: async pipe → p2u channel
    let mut t1 = tokio::spawn(async move {
        let mut buf = vec![0u8; 4096];
        loop {
            match pipe_r.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if p2u_tx.send(buf[..n].to_vec()).await.is_err() {
                        break;
                    }
                }
            }
        }
    });

    // t2: p2u channel → unix socket; clears activity and sends CLOSE when pipe closes
    let unix_w = unix.try_clone()?;
    let mut t2 = tokio::task::spawn_blocking(move || {
        while let Some(data) = p2u_rx.blocking_recv() {
            if sock_send_all(&unix_w, &data).is_err() {
                break;
            }
        }
        // Drain complete: game is done with RP. Explicitly clear the in-game activity
        // before sending CLOSE_FRAME. Discord does not reliably auto-clear on connection
        // close alone; we must send null to guarantee the activity is removed.
        if let Ok(null_payload) = serde_json::to_vec(&serde_json::json!({
            "cmd": "SET_ACTIVITY",
            "args": { "pid": 0u32, "activity": null },
            "nonce": "xbridge-clear"
        })) {
            let _ = sock_send_all(&unix_w, &make_frame(1, &null_payload));
        }
        let _ = sock_send_all(&unix_w, CLOSE_FRAME);
    });

    // t3: unix socket → u2p channel
    let unix_r = unix.try_clone()?;
    let mut t3 = tokio::task::spawn_blocking(move || {
        let mut buf = vec![MaybeUninit::<u8>::uninit(); 4096];
        loop {
            match unix_r.recv(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    // SAFETY: recv filled exactly n bytes
                    let data = unsafe {
                        std::slice::from_raw_parts(buf.as_ptr() as *const u8, n).to_vec()
                    };
                    if u2p_tx.blocking_send(data).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // t4: u2p channel → async pipe writer
    let mut t4 = tokio::spawn(async move {
        while let Some(data) = u2p_rx.recv().await {
            if pipe_w.write_all(&data).await.is_err() {
                break;
            }
        }
    });

    // Wait for any side to close. If Discord closes first, abort t1 to drop p2u_tx
    // and unblock t2's blocking_recv.
    tokio::select! {
        _ = &mut t1 => {}
        _ = &mut t2 => { t1.abort(); }
        _ = &mut t3 => { t1.abort(); }
        _ = &mut t4 => { t1.abort(); }
    }

    // Wait for t2 (sends SET_ACTIVITY(null) + CLOSE_FRAME) and t3 (Discord EOF).
    // This ensures the activity is cleared and the connection is fully closed before
    // we release the slot and allow discovery to reconnect.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let _ = t2.await;
        let _ = t3.await;
    })
    .await;

    // Force-close if tasks timed out (e.g. Discord was already gone)
    let _ = unix.shutdown(std::net::Shutdown::Both);

    slots.unmark(slot);
    Ok(())
}
