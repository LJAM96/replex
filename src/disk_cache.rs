use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::sync::Mutex;

/// On-disk record magic + version. Bumping this invalidates old cache files,
/// which is fine: the cache is disposable and callers refetch on parse failure.
const CACHE_MAGIC: &[u8; 4] = b"RPC1";

fn default_max_size() -> u64 {
    45 * 1024 * 1024 * 1024
}

fn clean_threshold(max: u64) -> u64 {
    // Keep a ~5 GiB buffer between the target and the high-water mark so the
    // cleaner does not run on every write once the cache is warm.
    max.saturating_sub(5 * 1024 * 1024 * 1024)
}

/// Maximum cache size, configurable via `REPLEX_DISK_CACHE_MAX_GB` (GiB).
/// Defaults to 45 GiB when unset or invalid.
fn max_size() -> u64 {
    static MAX: once_cell::sync::Lazy<u64> = once_cell::sync::Lazy::new(|| {
        match std::env::var("REPLEX_DISK_CACHE_MAX_GB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
        {
            Some(gb) if gb > 0 => gb * 1024 * 1024 * 1024,
            _ => default_max_size(),
        }
    });
    *MAX
}

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
    bytes
        .as_ref()
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect()
}

fn path_for_key(key: &str) -> PathBuf {
    let hash = hash_key(key);
    cache_dir().join(&hash[0..2]).join(hash)
}

/// A persisted cache record: the raw upstream payload plus the content type
/// Plex returned for it. Storing the content type means disk hits for photos
/// are semantically identical to the original response (WebP/PNG/etc.) rather
/// than always being mislabelled `image/jpeg`.
pub struct CacheRecord {
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

fn encode_record(content_type: Option<&str>, body: &[u8]) -> Vec<u8> {
    let ct = content_type.unwrap_or("");
    let mut out = Vec::with_capacity(15 + ct.len() + body.len());
    out.extend_from_slice(CACHE_MAGIC);
    let ct_len = ct.len() as u16;
    out.extend_from_slice(&ct_len.to_be_bytes());
    out.extend_from_slice(ct.as_bytes());
    out.extend_from_slice(&(body.len() as u64).to_be_bytes());
    out.extend_from_slice(body);
    out
}

fn decode_record(data: &[u8]) -> Option<CacheRecord> {
    if data.len() < 15 || &data[0..4] != CACHE_MAGIC {
        // Legacy raw payload (pre-record format): treat the whole file as the
        // body with no known content type. The caller refetches if needed.
        return Some(CacheRecord {
            content_type: None,
            body: data.to_vec(),
        });
    }
    let ct_len = u16::from_be_bytes([data[4], data[5]]) as usize;
    let ct_start = 6;
    let body_len_start = ct_start + ct_len;
    let body_len = u64::from_be_bytes([
        data[body_len_start],
        data[body_len_start + 1],
        data[body_len_start + 2],
        data[body_len_start + 3],
        data[body_len_start + 4],
        data[body_len_start + 5],
        data[body_len_start + 6],
        data[body_len_start + 7],
    ]) as usize;
    let body_start = body_len_start + 8;
    if data.len() < body_start + body_len {
        return None;
    }
    let content_type = if ct_len == 0 {
        None
    } else {
        String::from_utf8(data[ct_start..ct_start + ct_len].to_vec()).ok()
    };
    Some(CacheRecord {
        content_type,
        body: data[body_start..body_start + body_len].to_vec(),
    })
}

/// Read a raw cached body, ignoring any stored content type.
pub async fn get(key: &str) -> Option<Vec<u8>> {
    get_full(key).await.map(|r| r.body)
}

/// Read a full cache record (body + content type).
pub async fn get_full(key: &str) -> Option<CacheRecord> {
    let path = path_for_key(key);
    let data = tokio::fs::read(&path).await.ok()?;
    // Touch on read (LRU ordering) off the async runtime: set_file_mtime is a
    // synchronous syscall that would otherwise stall a Tokio worker.
    let touch = path.clone();
    let _ = tokio::task::spawn_blocking(move || {
        filetime::set_file_mtime(
            &touch,
            filetime::FileTime::from_system_time(std::time::SystemTime::now()),
        )
    })
    .await;
    decode_record(&data)
}

/// Write a raw body with no content type (e.g. JSON library payloads).
pub async fn put(key: &str, data: &[u8]) -> std::io::Result<()> {
    put_full(key, data, None).await
}

/// Write a body together with its content type. Writes go to a temporary file
/// in the same directory and are swapped into place with an atomic rename, so
/// a crash or concurrent read can never observe a half-written response.
pub async fn put_full(
    key: &str,
    data: &[u8],
    content_type: Option<&str>,
) -> std::io::Result<()> {
    let path = path_for_key(key);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    // Track overwrites correctly: subtract the previous entry's size before
    // swapping the new one in, so CURRENT_SIZE does not drift upwards.
    if let Ok(meta) = tokio::fs::metadata(&path).await {
        let old = meta.len();
        let _ = CURRENT_SIZE.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |s| Some(s.saturating_sub(old)),
        );
    }
    let encoded = encode_record(content_type, data);
    let tmp = path.with_extension("tmp");
    tokio::fs::write(&tmp, &encoded).await?;
    tokio::fs::rename(&tmp, &path).await?;
    CURRENT_SIZE.fetch_add(encoded.len() as u64, Ordering::Relaxed);
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
        let _ = CURRENT_SIZE.fetch_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |s| Some(s.saturating_sub(len)),
        );
    }
}

/// Skip stale `.tmp` files left by interrupted atomic writes.
fn is_cache_file(entry: &walkdir::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|n| !n.ends_with(".tmp"))
        .unwrap_or(false)
}

async fn ensure_capacity() {
    let size = CURRENT_SIZE.load(Ordering::Relaxed);
    if size < max_size() {
        return;
    }
    let _guard = CLEAN_LOCK.lock().await;
    if CURRENT_SIZE.load(Ordering::Relaxed) < max_size() {
        return;
    }
    let dir = cache_dir();
    // The directory scan is synchronous and can stall a Tokio worker for a
    // large cache; run it on a blocking thread and return the reconciled size
    // plus the (mtime, path, len) tuples to evict.
    let scanned = match tokio::task::spawn_blocking(move || {
        let mut files = Vec::new();
        let mut total: u64 = 0;
        for entry in walkdir::WalkDir::new(&dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file() && is_cache_file(&entry) {
                if let Ok(meta) = entry.metadata() {
                    let mtime = meta
                        .modified()
                        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
                    total += meta.len();
                    files.push((mtime, entry.path().to_path_buf(), meta.len()));
                }
            }
        }
        (files, total)
    })
    .await
    {
        Ok(scanned) => scanned,
        Err(_) => return,
    };
    let (mut files, total) = scanned;
    CURRENT_SIZE.store(total, Ordering::Relaxed);
    if total < clean_threshold(max_size()) {
        return;
    }
    files.sort_by_key(|(t, _, _)| *t);
    for (_, path, len) in files {
        if CURRENT_SIZE.load(Ordering::Relaxed) < clean_threshold(max_size()) {
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
    // Reconcile the tracked size from disk on a blocking thread so startup
    // never stalls the async runtime on a large existing cache.
    let total: u64 = match tokio::task::spawn_blocking(move || {
        let mut total: u64 = 0;
        for entry in walkdir::WalkDir::new(&dir)
            .into_iter()
            .filter_map(|e| e.ok())
        {
            if entry.file_type().is_file() && is_cache_file(&entry) {
                if let Ok(m) = entry.metadata() {
                    total += m.len();
                }
            }
        }
        total
    })
    .await
    {
        Ok(t) => t,
        Err(_) => 0,
    };
    CURRENT_SIZE.store(total, Ordering::Relaxed);
}
