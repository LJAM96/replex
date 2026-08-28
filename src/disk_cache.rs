use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;

const MAX_SIZE: u64 = 45 * 1024 * 1024 * 1024;
const CLEAN_THRESHOLD: u64 = 40 * 1024 * 1024 * 1024;

static CURRENT_SIZE: AtomicU64 = AtomicU64::new(0);
static CLEAN_LOCK: once_cell::sync::Lazy<Mutex<()>> =
    once_cell::sync::Lazy::new(|| Mutex::new(()));

fn cache_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("REPLEX_DISK_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    if Path::new("/data").exists() {
        PathBuf::from("/data/replex-cache")
    } else {
        PathBuf::from("./replex-cache")
    }
}

fn hash_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hex(hasher.finalize())
}

fn hex(bytes: impl AsRef<[u8]>) -> String {
    bytes.as_ref().iter().map(|b| format!("{:02x}", b)).collect()
}

fn path_for_key(key: &str) -> PathBuf {
    let hash = hash_key(key);
    cache_dir().join(&hash[0..2]).join(hash)
}

pub async fn get(key: &str) -> Option<Vec<u8>> {
    let path = path_for_key(key);
    let data = tokio::fs::read(&path).await.ok()?;
    let _ = filetime::set_file_mtime(
        &path,
        filetime::FileTime::from_system_time(std::time::SystemTime::now()),
    );
    Some(data)
}

pub async fn put(key: &str, data: &[u8]) -> std::io::Result<()> {
    let path = path_for_key(key);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // Track overwrites correctly: subtract the previous entry's size before
    // adding the new one so CURRENT_SIZE does not drift upwards until the
    // next full scan.
    if let Ok(meta) = tokio::fs::metadata(&path).await {
        let old = meta.len();
        let _ = CURRENT_SIZE.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |s| {
            Some(s.saturating_sub(old))
        });
    }
    tokio::fs::write(&path, data).await?;
    CURRENT_SIZE.fetch_add(data.len() as u64, Ordering::Relaxed);
    ensure_capacity().await;
    Ok(())
}

/// Remove a cache entry if present, keeping the tracked size consistent.
pub async fn remove(key: &str) {
    let path = path_for_key(key);
    let len = match tokio::fs::metadata(&path).await {
        Ok(m) => m.len(),
        Err(_) => return,
    };
    if tokio::fs::remove_file(&path).await.is_ok() {
        let _ = CURRENT_SIZE.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |s| {
            Some(s.saturating_sub(len))
        });
    }
}

async fn ensure_capacity() {
    let size = CURRENT_SIZE.load(Ordering::Relaxed);
    if size < MAX_SIZE {
        return;
    }
    let _guard = CLEAN_LOCK.lock().await;
    if CURRENT_SIZE.load(Ordering::Relaxed) < MAX_SIZE {
        return;
    }
    let dir = cache_dir();
    let mut files = Vec::new();
    let mut total: u64 = 0;
    let walker = walkdir::WalkDir::new(&dir).into_iter().filter_map(|e| e.ok());
    for entry in walker {
        if entry.file_type().is_file() {
            if let Ok(meta) = entry.metadata() {
                let mtime = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                total += meta.len();
                files.push((mtime, entry.path().to_path_buf(), meta.len()));
            }
        }
    }
    CURRENT_SIZE.store(total, Ordering::Relaxed);
    if total < CLEAN_THRESHOLD {
        return;
    }
    files.sort_by_key(|(t, _, _)| *t);
    for (_, path, len) in files {
        if CURRENT_SIZE.load(Ordering::Relaxed) < CLEAN_THRESHOLD {
            break;
        }
        let _ = tokio::fs::remove_file(&path).await;
        CURRENT_SIZE.fetch_sub(len, Ordering::Relaxed);
        let _ = tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

pub async fn init() {
    let dir = cache_dir();
    let _ = tokio::fs::create_dir_all(&dir).await;
    let mut total: u64 = 0;
    for entry in walkdir::WalkDir::new(&dir).into_iter().filter_map(|e| e.ok()) {
        if entry.file_type().is_file() {
            if let Ok(m) = entry.metadata() {
                total += m.len();
            }
        }
    }
    CURRENT_SIZE.store(total, Ordering::Relaxed);
}
