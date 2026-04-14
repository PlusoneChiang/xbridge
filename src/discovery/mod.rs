use crate::shared::{ActiveSessions, HandoffSignal, SlotRegistry};
use socket2::{Domain, SockAddr, Socket, Type};
use std::collections::HashMap;
use std::mem::MaybeUninit;
use tokio::sync::mpsc;

pub mod list_sync;
pub mod models;
mod process_scan;

use models::DetectableApp;

const SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

struct DiscoverySession {
    slot: u8,
    unix: Socket,
    pid: u32,
}

pub async fn run(
    slots: SlotRegistry,
    active: ActiveSessions,
    mut handoff_rx: mpsc::Receiver<HandoffSignal>,
    mut reclaim_rx: mpsc::Receiver<u8>,
) {
    match tokio::task::spawn_blocking(list_sync::sync).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => crate::log!("[discovery] list sync failed: {e}"),
        Err(e) => crate::log!("[discovery] list sync panic: {e}"),
    }

    let apps = match tokio::task::spawn_blocking(list_sync::load).await {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => {
            crate::log!("[discovery] failed to load detectable list: {e}; auto-discovery disabled");
            return;
        }
        Err(e) => {
            crate::log!("[discovery] list load panic: {e}; auto-discovery disabled");
            return;
        }
    };

    let exe_map: HashMap<String, usize> = apps
        .iter()
        .enumerate()
        .flat_map(|(i, app)| {
            app.executables
                .iter()
                .flatten()
                .filter(|e| !e.is_launcher)
                .map(move |e| (e.name.to_lowercase(), i))
        })
        .collect();

    crate::log!(
        "[discovery] loaded {} apps, {} exe entries in map",
        apps.len(),
        exe_map.len()
    );

    let mut sessions: HashMap<String, DiscoverySession> = HashMap::new();
    // app_id → (pid, slot): games handed off to bridge, tracked for exit and reclaim
    let mut handed_off: HashMap<String, (u32, u8)> = HashMap::new();
    let mut interval = tokio::time::interval(SCAN_INTERVAL);

    loop {
        tokio::select! {
            biased;
            Some(signal) = handoff_rx.recv() => {
                let entry = sessions.iter()
                    .find(|(_, s)| s.slot == signal.slot)
                    .map(|(cid, s)| (cid.clone(), s.pid));
                if let Some((cid, pid)) = entry {
                    crate::log!("[handoff] discovery releasing slot {} ({})", signal.slot, cid);
                    close_and_release(&cid, &slots, &active, &mut sessions, false);
                    handed_off.insert(cid, (pid, signal.slot));
                }
                let _ = signal.ack.send(());
            }
            Some(slot) = reclaim_rx.recv() => {
                // Bridge session ended — if this slot was handed off, let discovery resume
                let to_reclaim = handed_off.iter()
                    .find(|(_, (_, s))| *s == slot)
                    .map(|(cid, _)| cid.clone());
                if let Some(cid) = to_reclaim {
                    crate::log!("[discovery] slot {slot} reclaimed from bridge, resuming tracking");
                    handed_off.remove(&cid);
                    active.write().unwrap().remove(&cid);
                }
            }
            _ = interval.tick() => {
                scan_tick(&apps, &exe_map, &slots, &active, &mut sessions, &mut handed_off).await;
            }
        }
    }
}

async fn scan_tick(
    apps: &[DetectableApp],
    exe_map: &HashMap<String, usize>,
    slots: &SlotRegistry,
    active: &ActiveSessions,
    sessions: &mut HashMap<String, DiscoverySession>,
    handed_off: &mut HashMap<String, (u32, u8)>,
) {
    let processes = tokio::task::spawn_blocking(process_scan::scan)
        .await
        .unwrap_or_default();

    let running_pids: std::collections::HashSet<u32> =
        processes.iter().map(|(pid, _)| *pid).collect();

    // Release discovery sessions for dead games
    let dead: Vec<String> = sessions
        .iter()
        .filter(|(_, s)| !running_pids.contains(&s.pid))
        .map(|(cid, _)| cid.clone())
        .collect();
    for cid in dead {
        crate::log!("[discovery] game exited: {cid}");
        close_and_release(&cid, slots, active, sessions, true);
    }

    // Clean up handed-off games that have exited while bridge was managing them.
    // Bridge shuts down the socket before sending reclaim, so by the time we see the
    // PID gone here the Discord connection should already be closed. We still send
    // SET_ACTIVITY(null) via a fresh connection to be safe, in case Discord retained
    // the last in-game activity.
    let handed_off_dead: Vec<(String, u32)> = handed_off
        .iter()
        .filter(|(_, (pid, _))| !running_pids.contains(pid))
        .map(|(cid, (pid, _))| (cid.clone(), *pid))
        .collect();
    for (cid, pid) in handed_off_dead {
        crate::log!("[discovery] game exited (was handed off): {cid}");
        handed_off.remove(&cid);
        active.write().unwrap().remove(&cid);
        // Send SET_ACTIVITY(null) via a short-lived fresh connection so Discord
        // clears whatever activity the game last set through the bridge.
        if let Some(unix_path) = crate::bridge::resolve(0) {
            let cid_clone = cid.clone();
            tokio::task::spawn_blocking(move || {
                send_null_activity(&unix_path, &cid_clone, pid);
            });
        }
    }

    // Start sessions for newly detected games (skip if already managed or handed off)
    for (pid, exe) in &processes {
        let Some(&app_idx) = exe_map.get(exe.as_str()) else {
            continue;
        };
        let app = &apps[app_idx];
        if sessions.contains_key(&app.id) || active.read().unwrap().contains(&app.id) {
            continue;
        }
        let Some(slot) = slots.acquire() else {
            crate::log!("[discovery] no free slot for {}", app.name);
            continue;
        };
        match start_session(app, slot, *pid).await {
            Ok(session) => {
                active.write().unwrap().insert(app.id.clone());
                sessions.insert(app.id.clone(), session);
                crate::log!("[discovery] tracking {} (pid {pid})", app.name);
            }
            Err(e) => {
                slots.unmark(slot);
                crate::log!("[discovery] session start failed for {}: {e}", app.name);
            }
        }
    }
}

async fn start_session(
    app: &DetectableApp,
    slot: u8,
    pid: u32,
) -> anyhow::Result<DiscoverySession> {
    let unix_path = crate::bridge::resolve(slot)
        .ok_or_else(|| anyhow::anyhow!("no IPC path configured"))?;
    let client_id = app.id.clone();
    let app_name = app.name.clone();

    let unix = tokio::task::spawn_blocking(move || -> anyhow::Result<Socket> {
        use crate::bridge::{make_frame, sock_send_all};

        let sock = Socket::new(Domain::UNIX, Type::STREAM, None)?;
        sock.connect(&SockAddr::unix(&unix_path)?)?;

        // HANDSHAKE (opcode 0)
        let hs_payload = serde_json::to_vec(&serde_json::json!({ "v": 1, "client_id": client_id }))?;
        sock_send_all(&sock, &make_frame(0, &hs_payload))?;

        // Discard READY response
        let _ = recv_frame(&sock);

        // SET_ACTIVITY (opcode 1)
        let sa_payload = serde_json::to_vec(&serde_json::json!({
            "cmd": "SET_ACTIVITY",
            "args": { "pid": pid, "activity": { "details": app_name } },
            "nonce": "xbridge-1"
        }))?;
        sock_send_all(&sock, &make_frame(1, &sa_payload))?;

        Ok(sock)
    })
    .await??;

    // Drain incoming Discord events in background
    let drain = unix.try_clone()?;
    tokio::task::spawn_blocking(move || {
        let mut buf = [MaybeUninit::<u8>::uninit(); 256];
        while drain.recv(&mut buf).map(|n| n > 0).unwrap_or(false) {}
    });

    Ok(DiscoverySession { slot, unix, pid })
}

fn close_and_release(
    client_id: &str,
    slots: &SlotRegistry,
    active: &ActiveSessions,
    sessions: &mut HashMap<String, DiscoverySession>,
    remove_from_active: bool,
) {
    if let Some(s) = sessions.remove(client_id) {
        if let Ok(payload) = serde_json::to_vec(&serde_json::json!({
            "cmd": "SET_ACTIVITY",
            "args": { "pid": s.pid, "activity": null },
            "nonce": "xbridge-close"
        })) {
            if let Err(e) = crate::bridge::sock_send_all(&s.unix, &crate::bridge::make_frame(1, &payload)) {
                crate::log!("[discovery] SET_ACTIVITY(null) failed for {client_id}: {e}");
            }
        }
        let _ = crate::bridge::sock_send_all(&s.unix, crate::bridge::CLOSE_FRAME);
        let _ = s.unix.shutdown(std::net::Shutdown::Both);
        slots.unmark(s.slot);
    }
    if remove_from_active {
        active.write().unwrap().remove(client_id);
    }
}

/// Open a fresh Unix socket to Discord, send SET_ACTIVITY(null), then close.
/// Used to clear residual in-game activity after a bridge session ends.
fn send_null_activity(unix_path: &std::path::Path, client_id: &str, pid: u32) {
    use crate::bridge::{make_frame, sock_send_all, CLOSE_FRAME};
    let Ok(sock) = Socket::new(Domain::UNIX, Type::STREAM, None) else { return };
    let Ok(addr) = SockAddr::unix(unix_path) else { return };
    if sock.connect(&addr).is_err() { return; }

    let hs = serde_json::json!({ "v": 1, "client_id": client_id });
    let Ok(hs_bytes) = serde_json::to_vec(&hs) else { return };
    if sock_send_all(&sock, &make_frame(0, &hs_bytes)).is_err() { return; }

    // Discard READY (with timeout so we don't block forever)
    let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(1)));
    let mut buf = [MaybeUninit::<u8>::uninit(); 512];
    let _ = sock.recv(&mut buf);

    let null_act = serde_json::json!({
        "cmd": "SET_ACTIVITY",
        "args": { "pid": pid, "activity": null },
        "nonce": "xbridge-null"
    });
    let Ok(null_bytes) = serde_json::to_vec(&null_act) else { return };
    let _ = sock_send_all(&sock, &make_frame(1, &null_bytes));
    let _ = sock_send_all(&sock, CLOSE_FRAME);
    let _ = sock.shutdown(std::net::Shutdown::Both);
}

fn recv_frame(sock: &Socket) -> anyhow::Result<(u32, Vec<u8>)> {
    let mut header = [MaybeUninit::<u8>::uninit(); 8];
    sock_recv_exact(sock, &mut header)?;
    let header = unsafe { *(header.as_ptr() as *const [u8; 8]) };
    let opcode = u32::from_le_bytes(header[0..4].try_into().unwrap());
    let length = u32::from_le_bytes(header[4..8].try_into().unwrap());
    let mut payload = vec![MaybeUninit::<u8>::uninit(); length as usize];
    sock_recv_exact(sock, &mut payload)?;
    let payload = unsafe { std::slice::from_raw_parts(payload.as_ptr() as *const u8, length as usize).to_vec() };
    Ok((opcode, payload))
}

fn sock_recv_exact(sock: &Socket, buf: &mut [MaybeUninit<u8>]) -> std::io::Result<()> {
    let mut pos = 0;
    while pos < buf.len() {
        let n = sock.recv(&mut buf[pos..])?;
        if n == 0 {
            return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
        }
        pos += n;
    }
    Ok(())
}
