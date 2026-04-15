mod discord;
pub mod frame;
mod pipe;

use crate::shared::DiscoveryEvent;
use tokio::sync::mpsc;

const SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

#[allow(dead_code)]
struct GameInfo {
    app_id: String,
    app_name: String,
    pid: u32,
}

/// Gateway main loop.
///
/// Cycles through Idle → (Discovery | GameConnected) → Idle.
/// Pipe is always created (lazy-connect: Discord connection only opened when needed).
pub async fn run(mut discovery_rx: mpsc::Receiver<DiscoveryEvent>) {
    let mut cached_ready: Option<frame::Frame> = None;
    loop {
        // ── Idle: create pipe and wait for events ──
        let pipe = match pipe::create() {
            Ok(p) => p,
            Err(e) => {
                crate::log!("[gateway] pipe create failed: {e}");
                tokio::time::sleep(SCAN_INTERVAL).await;
                continue;
            }
        };
        crate::log!("[gateway] pipe ready, waiting for events");

        // Run active state (pipe exists, waiting for events)
        run_active_state(pipe, &mut discovery_rx, &mut cached_ready).await;
        // Returns here → back to Idle
    }
}

/// Pipe is up, waiting for Discovery events or game connections.
/// No Discord connection yet (lazy).
async fn run_active_state(
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    discovery_rx: &mut mpsc::Receiver<DiscoveryEvent>,
    cached_ready: &mut Option<frame::Frame>,
) {
    let pipe = pipe;
    loop {
        tokio::select! {
            ev = discovery_rx.recv() => {
                match ev {
                    Some(DiscoveryEvent::GameDetected { app_id, app_name, pid }) => {
                        run_discovery_state(
                            pipe, app_id, app_name, pid, discovery_rx, cached_ready,
                        ).await;
                        return;
                    }
                    Some(DiscoveryEvent::GameExited { .. }) => {} // ignore in Idle
                    None => {
                        return; // channel closed, shutdown
                    }
                }
            }
            result = pipe.connect() => {
                match result {
                    Ok(()) => {
                        // Game connected directly (no discovery)
                        run_game_state_fresh(pipe, None, discovery_rx, cached_ready).await;
                        return;
                    }
                    Err(e) => {
                        crate::log!("[gateway] pipe connect error: {e}");
                        return;
                    }
                }
            }
        }
    }
}

/// Discovery detected a game. Open Discord connection and set RP.
/// Stays in this state until game exits, game connects to pipe, or Discord disconnects.
async fn run_discovery_state(
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    app_id: String,
    app_name: String,
    pid: u32,
    discovery_rx: &mut mpsc::Receiver<DiscoveryEvent>,
    cached_ready: &mut Option<frame::Frame>,
) {
    let mut pipe = pipe;

    // Outer loop: handles Discord connect / reconnect with retry.
    'connect: loop {
        // Phase 1: Connect to Discord (retry until success, game exit, or pipe connect)
        let mut conn = loop {
            match discord::Connection::open(0, &app_id).await {
                Ok(c) => break c,
                Err(e) => crate::log!("[gateway] discord connect failed: {e}"),
            }
            // Wait before retry, but keep handling other events
            tokio::select! {
                ev = discovery_rx.recv() => {
                    match ev {
                        Some(DiscoveryEvent::GameExited { .. }) | None => return,
                        Some(DiscoveryEvent::GameDetected { .. }) => {}
                    }
                }
                result = pipe.connect() => {
                    match result {
                        Ok(()) => {
                            crate::log!("[gateway] game connected to pipe, handing off");
                            let gi = GameInfo { app_id: app_id.clone(), app_name: app_name.clone(), pid };
                            let still_running = run_game_state_fresh(pipe, Some(gi), discovery_rx, cached_ready).await;
                            if !still_running { return; }
                            pipe = match pipe::create() {
                                Ok(p) => p,
                                Err(_) => return,
                            };
                        }
                        Err(e) => {
                            crate::log!("[gateway] pipe connect error: {e}");
                            return;
                        }
                    }
                }
                _ = tokio::time::sleep(SCAN_INTERVAL) => {}
            }
        };

        // Update gateway cached_ready
        if let Some(ready) = &conn.cached_ready {
            *cached_ready = Some(ready.clone());
        }

        crate::log!("[gateway] discovery active: {app_name} (pid {pid})");

        // Send discovery activity
        if let Err(e) = conn.send_discovery_activity(&app_name, pid).await {
            crate::log!("[gateway] discovery set_activity failed: {e}");
            conn.clear_and_close().await;
            continue 'connect;
        }

        // Start reader to monitor Discord connection health
        let mut discord_rx_ch = match conn.start_reader() {
            Ok(rx) => rx,
            Err(e) => {
                crate::log!("[gateway] failed to start discord reader: {e}");
                conn.clear_and_close().await;
                continue 'connect;
            }
        };

        // Phase 2: Monitor — watch for game exit, pipe connect, Discord disconnect
        loop {
            tokio::select! {
                ev = discovery_rx.recv() => {
                    match ev {
                        Some(DiscoveryEvent::GameExited { .. }) | None => {
                            crate::log!("[gateway] game exited, clearing activity");
                            conn.clear_and_close().await;
                            return;
                        }
                        Some(DiscoveryEvent::GameDetected { .. }) => {} // ignore duplicate
                    }
                }
                result = pipe.connect() => {
                    match result {
                        Ok(()) => {
                            crate::log!("[gateway] game connected to pipe, handing off");
                            let game_info = GameInfo {
                                app_id: app_id.clone(),
                                app_name: app_name.clone(),
                                pid,
                            };
                            let game_still_running = run_game_state_with_conn(
                                pipe, &mut conn, Some(game_info), discovery_rx, cached_ready,
                            ).await;

                            if !game_still_running {
                                crate::log!("[gateway] game exited, clearing activity");
                                conn.clear_and_close().await;
                                return;
                            }

                            // Game RP disconnected but process still running.
                            // Always reconnect (proxy_loop's reader holds a cloned socket fd).
                            pipe = match pipe::create() {
                                Ok(p) => p,
                                Err(_) => {
                                    conn.clear_and_close().await;
                                    return;
                                }
                            };
                            conn.clear_and_close().await;
                            continue 'connect;
                        }
                        Err(e) => {
                            crate::log!("[gateway] pipe connect error: {e}");
                            conn.clear_and_close().await;
                            return;
                        }
                    }
                }
                msg = discord_rx_ch.recv() => {
                    let need_reconnect = match msg {
                        Some(Ok(f)) if f.opcode == frame::CLOSE => {
                            crate::log!("[gateway] discord sent CLOSE during discovery");
                            true
                        }
                        Some(Ok(_)) => false, // ignore other frames
                        _ => {
                            crate::log!("[gateway] discord disconnected during discovery");
                            true
                        }
                    };
                    if need_reconnect {
                        conn.clear_and_close().await;
                        continue 'connect;
                    }
                }
            }
        }
    }
}

/// Game connected directly (no prior Discovery connection).
/// Must create a fresh Discord connection using the game's client_id.
/// Retries Discord connection while keeping the pipe alive.
async fn run_game_state_fresh(
    mut pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    game_info: Option<GameInfo>,
    discovery_rx: &mut mpsc::Receiver<DiscoveryEvent>,
    cached_ready: &mut Option<frame::Frame>,
) -> bool {
    // Read game HANDSHAKE from pipe
    let handshake = match frame::read(&mut pipe).await {
        Ok(f) if f.opcode == frame::HANDSHAKE => f,
        Ok(_) => return false,
        Err(_) => return false,
    };

    let client_id = match extract_client_id(&handshake.payload) {
        Some(id) => id,
        None => return false,
    };

    // Connect to Discord with retry (keeps pipe alive, facade mode if cached READY available)
    let (mut conn, ready_sent, last_game_frame) = match connect_discord_with_retry(
        &client_id, &mut pipe, discovery_rx, cached_ready,
    ).await {
        RetryConnectResult::Connected { conn, ready_sent, last_game_frame } => {
            (conn, ready_sent, last_game_frame)
        }
        RetryConnectResult::PipeClosed => return game_info.is_some(),
        RetryConnectResult::GameExited => return false,
    };

    // Update gateway cached_ready
    if let Some(ready) = &conn.cached_ready {
        *cached_ready = Some(ready.clone());
    }

    // Send READY to game (if not already sent via facade)
    if !ready_sent {
        if let Some(ready) = &conn.cached_ready {
            if let Err(e) = frame::write(&mut pipe, ready).await {
                crate::log!("[gateway] failed to send READY to game: {e}");
                conn.clear_and_close().await;
                return game_info.is_some();
            }
        }
    }

    // Replay buffered game frame (accumulated during facade mode)
    if let Some(f) = &last_game_frame {
        if let Err(e) = conn.write_frame(f).await {
            crate::log!("[gateway] failed to replay buffered frame: {e}");
            conn.clear_and_close().await;
            return game_info.is_some();
        }
    }

    crate::log!("[gateway] game session active (client_id: {client_id})");

    // Proxy loop
    let result = proxy_loop(&mut pipe, &mut conn, discovery_rx, cached_ready).await;
    conn.clear_and_close().await;
    result
}

/// Game connected while Discovery was active. May reuse existing connection.
async fn run_game_state_with_conn(
    mut pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    conn: &mut discord::Connection,
    game_info: Option<GameInfo>,
    discovery_rx: &mut mpsc::Receiver<DiscoveryEvent>,
    cached_ready: &mut Option<frame::Frame>,
) -> bool {
    // Read game HANDSHAKE from pipe
    let handshake = match frame::read(&mut pipe).await {
        Ok(f) if f.opcode == frame::HANDSHAKE => f,
        Ok(_) => return false,
        Err(_) => return false,
    };

    let game_client_id = match extract_client_id(&handshake.payload) {
        Some(id) => id,
        None => return false,
    };

    if game_client_id == conn.client_id {
        // Same app → reuse connection
        crate::log!("[gateway] reusing connection (same client_id: {game_client_id})");
        let _ = conn.send_null_activity().await;

        // Send cached READY to game
        if let Some(ready) = &conn.cached_ready {
            if let Err(e) = frame::write(&mut pipe, ready).await {
                crate::log!("[gateway] failed to send cached READY: {e}");
                return game_info.is_some();
            }
        }
    } else {
        // Different app → must reconnect (with retry)
        crate::log!("[gateway] reconnecting (client_id changed: {} → {})", conn.client_id, game_client_id);
        conn.clear_and_close().await;

        let (new_conn, ready_sent, last_game_frame) = match connect_discord_with_retry(
            &game_client_id, &mut pipe, discovery_rx, cached_ready,
        ).await {
            RetryConnectResult::Connected { conn, ready_sent, last_game_frame } => {
                (conn, ready_sent, last_game_frame)
            }
            RetryConnectResult::PipeClosed => return game_info.is_some(),
            RetryConnectResult::GameExited => return false,
        };
        *conn = new_conn;

        // Update gateway cached_ready
        if let Some(ready) = &conn.cached_ready {
            *cached_ready = Some(ready.clone());
        }

        // Send READY to game (if not already sent via facade)
        if !ready_sent {
            if let Some(ready) = &conn.cached_ready {
                if let Err(e) = frame::write(&mut pipe, ready).await {
                    crate::log!("[gateway] failed to send READY: {e}");
                    return game_info.is_some();
                }
            }
        }

        // Replay buffered game frame
        if let Some(f) = &last_game_frame {
            if let Err(e) = conn.write_frame(f).await {
                crate::log!("[gateway] failed to replay buffered frame: {e}");
                return game_info.is_some();
            }
        }
    }

    crate::log!("[gateway] game session active (client_id: {})", conn.client_id);

    // Proxy loop
    proxy_loop(&mut pipe, conn, discovery_rx, cached_ready).await
}

enum RetryConnectResult {
    Connected {
        conn: discord::Connection,
        ready_sent: bool,
        last_game_frame: Option<frame::Frame>,
    },
    PipeClosed,
    GameExited,
}

/// Try to connect to Discord, retrying until success while keeping the pipe alive.
/// On first failure, sends gateway-level cached READY to game (facade mode) if available.
async fn connect_discord_with_retry(
    client_id: &str,
    pipe: &mut tokio::net::windows::named_pipe::NamedPipeServer,
    discovery_rx: &mut mpsc::Receiver<DiscoveryEvent>,
    cached_ready: &Option<frame::Frame>,
) -> RetryConnectResult {
    let mut last_game_frame: Option<frame::Frame> = None;
    let mut ready_sent = false;

    loop {
        match discord::Connection::open(0, client_id).await {
            Ok(c) => {
                return RetryConnectResult::Connected {
                    conn: c,
                    ready_sent,
                    last_game_frame,
                };
            }
            Err(e) => {
                crate::log!("[gateway] discord connect failed: {e}");
            }
        }

        // Send cached READY to game on first failure (facade mode)
        if !ready_sent {
            if let Some(ready) = cached_ready {
                if frame::write(pipe, ready).await.is_err() {
                    return RetryConnectResult::PipeClosed;
                }
                crate::log!("[gateway] sent cached READY (discord unavailable, buffering)");
                ready_sent = true;
            }
        }

        // Wait before retry, but keep the pipe alive (drain game frames)
        tokio::select! {
            result = frame::read(pipe) => {
                match result {
                    Ok(f) => {
                        if f.opcode == frame::FRAME {
                            last_game_frame = Some(f);
                        }
                    }
                    Err(_) => return RetryConnectResult::PipeClosed,
                }
            }
            ev = discovery_rx.recv() => {
                match ev {
                    Some(DiscoveryEvent::GameExited { .. }) | None => {
                        return RetryConnectResult::GameExited;
                    }
                    Some(DiscoveryEvent::GameDetected { .. }) => {}
                }
            }
            _ = tokio::time::sleep(SCAN_INTERVAL) => {}
        }
    }
}

/// Retry Discord reconnection while keeping the game pipe alive.
/// Drains incoming game frames (updating the frame cache) while retrying.
/// Returns true on success, false if the game disconnected or exited.
async fn retry_reconnect_discord(
    conn: &mut discord::Connection,
    discord_rx: &mut mpsc::Receiver<std::io::Result<frame::Frame>>,
    last_game_frame: &mut Option<frame::Frame>,
    pipe: &mut tokio::net::windows::named_pipe::NamedPipeServer,
    game_exited: &mut bool,
    discovery_rx: &mut mpsc::Receiver<DiscoveryEvent>,
    cached_ready: &mut Option<frame::Frame>,
) -> bool {
    conn.clear_and_close().await;

    loop {
        let cid = conn.client_id.clone();
        match discord::Connection::open(0, &cid).await {
            Ok(new_conn) => {
                *conn = new_conn;
                // Update gateway cached_ready
                if let Some(ready) = &conn.cached_ready {
                    *cached_ready = Some(ready.clone());
                }
                let mut ok = true;
                if let Some(f) = last_game_frame.as_ref() {
                    if let Err(e) = conn.write_frame(f).await {
                        crate::log!("[gateway] replay failed after reconnect: {e}");
                        ok = false;
                    }
                }
                if ok {
                    match conn.start_reader() {
                        Ok(rx) => {
                            *discord_rx = rx;
                            crate::log!("[gateway] discord reconnected (client_id: {cid})");
                            return true;
                        }
                        Err(e) => {
                            crate::log!("[gateway] reader start failed: {e}");
                        }
                    }
                }
                conn.clear_and_close().await;
            }
            Err(e) => {
                crate::log!("[gateway] discord reconnect failed: {e}");
            }
        }

        // Wait before retry, but keep the pipe alive (drain game frames)
        tokio::select! {
            result = frame::read(pipe) => {
                match result {
                    Ok(f) => {
                        if f.opcode == frame::FRAME {
                            *last_game_frame = Some(f);
                        }
                    }
                    Err(e) => {
                        crate::log!("[gateway] pipe read error during discord reconnect: {e}");
                        return false;
                    }
                }
            }
            ev = discovery_rx.recv() => {
                match ev {
                    Some(DiscoveryEvent::GameExited { .. }) | None => {
                        *game_exited = true;
                        return false;
                    }
                    Some(DiscoveryEvent::GameDetected { .. }) => {}
                }
            }
            _ = tokio::time::sleep(SCAN_INTERVAL) => {}
        }
    }
}

/// Bidirectional proxy between game pipe and Discord.
/// Returns true if the game is still running when the proxy ends.
async fn proxy_loop(
    pipe: &mut tokio::net::windows::named_pipe::NamedPipeServer,
    conn: &mut discord::Connection,
    discovery_rx: &mut mpsc::Receiver<DiscoveryEvent>,
    cached_ready: &mut Option<frame::Frame>,
) -> bool {
    let mut game_exited = false;
    let mut last_game_frame: Option<frame::Frame> = None;

    // Spawn a dedicated reader task so Discord reads don't race inside select!
    let mut discord_rx = match conn.start_reader() {
        Ok(rx) => rx,
        Err(e) => {
            crate::log!("[gateway] failed to start discord reader: {e}");
            return false;
        }
    };

    loop {
        tokio::select! {
            result = frame::read(pipe) => {
                match result {
                    Ok(f) => {
                        if f.opcode == frame::FRAME {
                            last_game_frame = Some(f.clone());
                        }
                        if let Err(e) = conn.write_frame(&f).await {
                            crate::log!("[gateway] pipe→discord write error: {e}, reconnecting");
                            if !retry_reconnect_discord(
                                conn, &mut discord_rx, &mut last_game_frame,
                                pipe, &mut game_exited, discovery_rx, cached_ready,
                            ).await {
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        crate::log!("[gateway] pipe read error (game disconnected): {e}");
                        break;
                    }
                }
            }
            msg = discord_rx.recv() => {
                match msg {
                    Some(Ok(f)) => {
                        if f.opcode == frame::CLOSE {
                            let _ = frame::write(pipe, &f).await;
                            crate::log!("[gateway] discord sent CLOSE frame");
                            break;
                        }
                        if let Err(e) = frame::write(pipe, &f).await {
                            crate::log!("[gateway] discord→pipe write error: {e}");
                            break;
                        }
                    }
                    Some(Err(e)) => {
                        crate::log!("[gateway] discord disconnected: {e}, reconnecting");
                        if !retry_reconnect_discord(
                            conn, &mut discord_rx, &mut last_game_frame,
                            pipe, &mut game_exited, discovery_rx, cached_ready,
                        ).await {
                            break;
                        }
                    }
                    None => {
                        crate::log!("[gateway] discord reader closed, reconnecting");
                        if !retry_reconnect_discord(
                            conn, &mut discord_rx, &mut last_game_frame,
                            pipe, &mut game_exited, discovery_rx, cached_ready,
                        ).await {
                            break;
                        }
                    }
                }
            }
            ev = discovery_rx.recv() => {
                match ev {
                    Some(DiscoveryEvent::GameExited { .. }) => {
                        game_exited = true;
                    }
                    Some(DiscoveryEvent::GameDetected { .. }) => {} // ignore
                    None => {
                        game_exited = true;
                        break; // channel closed
                    }
                }
            }
        }
    }

    !game_exited
}

/// Extract client_id from a HANDSHAKE JSON payload.
fn extract_client_id(payload: &[u8]) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(payload).ok()?;
    v.get("client_id")?.as_str().map(|s| s.to_owned())
}
