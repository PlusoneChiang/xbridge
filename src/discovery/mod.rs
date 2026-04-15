use crate::shared::DiscoveryEvent;
use std::collections::HashMap;
use tokio::sync::mpsc;

pub mod list_sync;
pub mod models;
mod process_scan;

const SCAN_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

/// Pure process scanner. Reports GameDetected / GameExited events via channel.
/// No IPC connections — Gateway owns all Discord communication.
pub async fn run(event_tx: mpsc::Sender<DiscoveryEvent>) {
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

    // Only track one game at a time (first detected wins)
    let mut tracked: Option<(String, u32)> = None; // (app_id, pid)
    let mut interval = tokio::time::interval(SCAN_INTERVAL);

    loop {
        interval.tick().await;

        let processes = tokio::task::spawn_blocking(process_scan::scan)
            .await
            .unwrap_or_default();

        let running_pids: std::collections::HashSet<u32> =
            processes.iter().map(|(pid, _)| *pid).collect();

        // Check if tracked game exited
        if let Some((ref app_id, pid)) = tracked {
            if !running_pids.contains(&pid) {
                crate::log!("[discovery] game exited: {app_id}");
                let _ = event_tx
                    .send(DiscoveryEvent::GameExited {
                        app_id: app_id.clone(),
                    })
                    .await;
                tracked = None;
            }
        }

        // If nothing tracked, look for a new game
        if tracked.is_none() {
            for (pid, exe) in &processes {
                let Some(&app_idx) = exe_map.get(exe.as_str()) else {
                    continue;
                };
                let app = &apps[app_idx];

                crate::log!("[discovery] tracking {} (pid {pid})", app.name);
                let _ = event_tx
                    .send(DiscoveryEvent::GameDetected {
                        app_id: app.id.clone(),
                        app_name: app.name.clone(),
                        pid: *pid,
                    })
                    .await;
                tracked = Some((app.id.clone(), *pid));
                break; // only track first detected
            }
        }
    }
}

