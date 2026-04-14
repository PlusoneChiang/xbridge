use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::PathBuf;

const REMOTE_LIST_URL: &str =
    "https://raw.githubusercontent.com/PlusoneChiang/xbridge/main/xbridge-detectable-list.json";
const REMOTE_HASH_URL: &str =
    "https://raw.githubusercontent.com/PlusoneChiang/xbridge/main/xbridge-detectable-list.json.sha256";

fn list_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .fold(String::with_capacity(64), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// Download the detectable list without checking local state (used on first install).
pub fn download_fresh() -> anyhow::Result<()> {
    let list_bytes = fetch_bytes(REMOTE_LIST_URL)?;
    let expected = fetch_string(REMOTE_HASH_URL)?;
    verify_and_save(&list_bytes, &expected)
}

/// Hash-first sync: only download list if remote hash differs from local.
/// Called at service startup.
pub fn sync() -> anyhow::Result<()> {
    let dir = list_dir();
    let hash_path = dir.join("xbridge-detectable-list.json.sha256");

    let remote_hash = fetch_string(REMOTE_HASH_URL)?;
    let remote_hash = remote_hash.trim();

    if let Ok(local_hash) = std::fs::read_to_string(&hash_path) {
        if local_hash.trim() == remote_hash {
            return Ok(()); // up to date
        }
    }

    let list_bytes = fetch_bytes(REMOTE_LIST_URL)?;
    verify_and_save(&list_bytes, remote_hash)?;
    crate::log!("[sync] detectable list updated");
    Ok(())
}

/// Load the local detectable list from disk.
pub fn load() -> anyhow::Result<Vec<crate::discovery::models::DetectableApp>> {
    let path = list_dir().join("xbridge-detectable-list.json");
    let data = std::fs::read(&path)?;
    Ok(serde_json::from_slice(&data)?)
}

fn fetch_bytes(url: &str) -> anyhow::Result<Vec<u8>> {
    use std::io::Read;
    let response = ureq::get(url).set("User-Agent", "xbridge/1.0").call()?;
    let mut bytes = Vec::new();
    response.into_reader().read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn fetch_string(url: &str) -> anyhow::Result<String> {
    let s = ureq::get(url)
        .set("User-Agent", "xbridge/1.0")
        .call()?
        .into_string()?;
    Ok(s)
}

fn verify_and_save(list_bytes: &[u8], expected_hash: &str) -> anyhow::Result<()> {
    let actual = sha256_hex(list_bytes);
    if actual != expected_hash.trim() {
        anyhow::bail!("hash mismatch: expected {expected_hash}, got {actual}");
    }
    let dir = list_dir();
    std::fs::write(dir.join("xbridge-detectable-list.json"), list_bytes)?;
    std::fs::write(
        dir.join("xbridge-detectable-list.json.sha256"),
        expected_hash.trim(),
    )?;
    Ok(())
}
