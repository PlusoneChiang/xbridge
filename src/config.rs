use std::path::PathBuf;

/// Resolves the Discord IPC socket directory, reading fresh on every call.
///
/// Priority:
/// 1. env `DISCORD_IPC_PATH`  — works on Linux (env passes through Wine)
/// 2. registry `HKLM\SYSTEM\CurrentControlSet\Control\Session Manager\Environment\DISCORD_IPC_PATH`
///    — required on macOS where Wine services cannot inherit POSIX env vars
/// 3. env `XDG_RUNTIME_DIR`   — Linux fallback
pub fn resolve_socket_dir() -> Option<PathBuf> {
    // 1. env DISCORD_IPC_PATH
    if let Ok(p) = std::env::var("DISCORD_IPC_PATH") {
        if p.starts_with('/') || p.starts_with('\\') {
            let path = PathBuf::from(&p);
            crate::log!("[config] IPC path from env: {}", path.display());
            return Some(path);
        }
        crate::log!("[config] env DISCORD_IPC_PATH is not a valid path: {p}");
    }

    // 2. Registry DISCORD_IPC_PATH (macOS: env unavailable in Wine service)
    if let Some(p) = read_registry_ipc_path() {
        if p.starts_with('/') || p.starts_with('\\') {
            let path = PathBuf::from(&p);
            crate::log!("[config] IPC path from registry: {}", path.display());
            return Some(path);
        }
        crate::log!("[config] registry DISCORD_IPC_PATH is not a valid path: {p}");
    }

    // 3. XDG_RUNTIME_DIR (Linux fallback)
    if let Ok(p) = std::env::var("XDG_RUNTIME_DIR") {
        crate::log!("[config] IPC path from XDG_RUNTIME_DIR: {p}");
        return Some(PathBuf::from(p));
    }

    crate::log!("[config] no IPC path found (env/registry/XDG all unset)");
    None
}

fn read_registry_ipc_path() -> Option<String> {
    use windows_sys::Win32::System::Registry::{
        RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY_LOCAL_MACHINE, KEY_READ,
    };

    let key_path: Vec<u16> =
        "SYSTEM\\CurrentControlSet\\Control\\Session Manager\\Environment\0"
            .encode_utf16()
            .collect();
    let value_name: Vec<u16> = "DISCORD_IPC_PATH\0".encode_utf16().collect();

    unsafe {
        let mut hkey = 0isize;
        let open_ret = RegOpenKeyExW(HKEY_LOCAL_MACHINE, key_path.as_ptr(), 0, KEY_READ, &mut hkey);
        if open_ret != 0 {
            crate::log!("[config] registry open failed (code {open_ret})");
            return None;
        }

        let mut buf = vec![0u16; 1024];
        let mut len = (buf.len() * 2) as u32;
        let mut kind = 0u32;
        let ret = RegQueryValueExW(
            hkey,
            value_name.as_ptr(),
            std::ptr::null(),
            &mut kind,
            buf.as_mut_ptr() as *mut u8,
            &mut len,
        );
        RegCloseKey(hkey);

        if ret != 0 {
            crate::log!("[config] registry query failed (code {ret})");
            return None;
        }
        if len < 2 {
            crate::log!("[config] registry value empty (len={len})");
            return None;
        }

        // len is in bytes; convert to u16 count and strip null terminator
        let wchars = (len / 2) as usize;
        let s = String::from_utf16_lossy(&buf[..wchars.saturating_sub(1)]).to_owned();
        if s.is_empty() { None } else { Some(s) }
    }
}
