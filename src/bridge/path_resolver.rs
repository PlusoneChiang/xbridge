use std::path::PathBuf;

/// Returns the native Unix socket path for a given slot, resolved fresh on every call.
pub fn resolve(slot: u8) -> Option<PathBuf> {
    crate::config::resolve_socket_dir()
        .map(|dir| dir.join(format!("discord-ipc-{slot}")))
}
