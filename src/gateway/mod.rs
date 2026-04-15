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
        run_active_state(pipe, &mut discovery_rx).await;
        // Returns here → back to Idle
    }
}

/// Pipe is up, waiting for Discovery events or game connections.
/// No Discord connection yet (lazy).
async fn run_active_state(
    pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    discovery_rx: &mut mpsc::Receiver<DiscoveryEvent>,
) {
    let pipe = pipe;
    loop {
        tokio::select! {
            ev = discovery_rx.recv() => {
                match ev {
                    Some(DiscoveryEvent::GameDetected { app_id, app_name, pid }) => {
                        run_discovery_state(
                            pipe, app_id, app_name, pid, discovery_rx,
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
                        run_game_state_fresh(pipe, None, discovery_rx).await;
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
) {
    // Lazy connect to Discord
    let mut conn = match discord::Connection::open(0, &app_id).await {
        Ok(c) => c,
        Err(e) => {
            crate::log!("[gateway] discord connect failed: {e}");
            return; // → Idle
        }
    };
    crate::log!("[gateway] discovery active: {app_name} (pid {pid})");

    // Send discovery activity
    if let Err(e) = conn.send_discovery_activity(&app_name, pid).await {
        crate::log!("[gateway] discovery set_activity failed: {e}");
        conn.clear_and_close().await;
        return;
    }

    let mut pipe = pipe;

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
                            pipe, &mut conn, Some(game_info), discovery_rx,
                        ).await;

                        if !game_still_running {
                            crate::log!("[gateway] game exited, clearing activity");
                            conn.clear_and_close().await;
                            return;
                        }

                        // Game RP disconnected but process still running.
                        // Discovery resumes immediately.
                        pipe = match pipe::create() {
                            Ok(p) => p,
                            Err(_) => {
                                conn.clear_and_close().await;
                                return;
                            }
                        };

                        if conn.client_id == app_id {
                            let _ = conn.send_null_activity().await;
                            if let Err(e) = conn.send_discovery_activity(&app_name, pid).await {
                                crate::log!("[gateway] discovery re-set failed: {e}");
                                conn.clear_and_close().await;
                                return;
                            }
                            crate::log!("[gateway] discovery resumed (reused connection)");
                        } else {
                            conn.clear_and_close().await;
                            conn = match discord::Connection::open(0, &app_id).await {
                                Ok(c) => c,
                                Err(e) => {
                                    crate::log!("[gateway] discord reconnect for discovery failed: {e}");
                                    return;
                                }
                            };
                            if let Err(e) = conn.send_discovery_activity(&app_name, pid).await {
                                crate::log!("[gateway] discovery re-set failed: {e}");
                                conn.clear_and_close().await;
                                return;
                            }
                            crate::log!("[gateway] discovery resumed (new connection)");
                        }
                        continue; // back to discovery loop
                    }
                    Err(e) => {
                        crate::log!("[gateway] pipe connect error: {e}");
                        conn.clear_and_close().await;
                        return;
                    }
                }
            }
        }
    }
}

/// Game connected directly (no prior Discovery connection).
/// Must create a fresh Discord connection using the game's client_id.
async fn run_game_state_fresh(
    mut pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    game_info: Option<GameInfo>,
    discovery_rx: &mut mpsc::Receiver<DiscoveryEvent>,
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

    // Connect to Discord with game's client_id
    let conn = match discord::Connection::open(0, &client_id).await {
        Ok(c) => c,
        Err(e) => {
            crate::log!("[gateway] discord connect for game failed: {e}");
            return game_info.is_some();
        }
    };

    // Forward cached READY to game
    if let Some(ready) = &conn.cached_ready {
        if let Err(e) = frame::write(&mut pipe, ready).await {
            crate::log!("[gateway] failed to send READY to game: {e}");
            conn.clear_and_close().await;
            return game_info.is_some();
        }
    }

    crate::log!("[gateway] game session active (client_id: {client_id})");

    // Proxy loop
    let result = proxy_loop(&mut pipe, &conn, discovery_rx).await;
    conn.clear_and_close().await;
    result
}

/// Game connected while Discovery was active. May reuse existing connection.
async fn run_game_state_with_conn(
    mut pipe: tokio::net::windows::named_pipe::NamedPipeServer,
    conn: &mut discord::Connection,
    game_info: Option<GameInfo>,
    discovery_rx: &mut mpsc::Receiver<DiscoveryEvent>,
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
        // Different app → must reconnect
        crate::log!("[gateway] reconnecting (client_id changed: {} → {})", conn.client_id, game_client_id);
        conn.clear_and_close().await;

        let new_conn = match discord::Connection::open(0, &game_client_id).await {
            Ok(c) => c,
            Err(e) => {
                crate::log!("[gateway] discord reconnect failed: {e}");
                return game_info.is_some();
            }
        };
        *conn = new_conn;

        // Forward cached READY to game
        if let Some(ready) = &conn.cached_ready {
            if let Err(e) = frame::write(&mut pipe, ready).await {
                crate::log!("[gateway] failed to send READY: {e}");
                return game_info.is_some();
            }
        }
    }

    crate::log!("[gateway] game session active (client_id: {})", conn.client_id);

    // Proxy loop
    proxy_loop(&mut pipe, conn, discovery_rx).await
}

/// Bidirectional proxy between game pipe and Discord.
/// Returns true if the game is still running when the proxy ends.
async fn proxy_loop(
    pipe: &mut tokio::net::windows::named_pipe::NamedPipeServer,
    conn: &discord::Connection,
    discovery_rx: &mut mpsc::Receiver<DiscoveryEvent>,
) -> bool {
    let mut game_exited = false;

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
                        if let Err(e) = conn.write_frame(&f).await {
                            crate::log!("[gateway] pipe→discord write error: {e}");
                            break;
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
                        crate::log!("[gateway] discord read error: {e}");
                        break;
                    }
                    None => {
                        crate::log!("[gateway] discord reader closed");
                        break;
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
