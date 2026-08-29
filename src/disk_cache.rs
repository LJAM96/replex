use sha2::{Digest, Sha256};
use std::io::{Error, ErrorKind};
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
    // Clean back to 85% of the configured high-water mark. A proportional
    // target behaves sensibly for both the default 45 GiB cache and small
    // deployments where subtracting a fixed 5 GiB could otherwise reduce the
    // target to zero and wipe the entire cache.
    max.saturating_mul(85) / 100
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
static WRITE_COUNTER: AtomicU64 = AtomicU64::new(0);
static CLEAN_LOCK: once_cell::sync::Lazy<Mutex<()>> =
    once_cell::sync::Lazy::new(|| Mutex::new(()));
// Temp payloads can be written concurrently, but the final metadata/rename
// and size-accounting transition is serialised so two writers replacing the
// same key cannot make CURRENT_SIZE drift.
static COMMIT_LOCK: once_cell::sync::Lazy<Mutex<()>> =
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

fn encode_record(
    content_type: Option<&str>,
    body: &[u8],
) -> std::io::Result<Vec<u8>> {
    let ct = content_type.unwrap_or("");
    let mut out = Vec::with_capacity(15 + ct.len() + body.len());
    out.extend_from_slice(CACHE_MAGIC);
    let ct_len = u16::try_from(ct.len()).map_err(|_| {
        Error::new(
            ErrorKind::InvalidInput,
            "cache content type is too large to encode",
        )
    })?;
    out.extend_from_slice(&ct_len.to_be_bytes());
    out.extend_from_slice(ct.as_bytes());
    let body_len = u64::try_from(body.len()).map_err(|_| {
        Error::new(ErrorKind::InvalidInput, "cache body is too large to encode")
    })?;
    out.extend_from_slice(&body_len.to_be_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

fn decode_record(data: &[u8]) -> Option<CacheRecord> {
    // Files from the pre-record cache format did not have a magic header and
    // remain readable as a raw payload. Once the current magic is present,
    // however, every following length is untrusted and a truncated record is
    // corruption rather than a legacy payload.
    if data.len() < CACHE_MAGIC.len() || &data[0..4] != CACHE_MAGIC {
        // Legacy raw payload (pre-record format): treat the whole file as the
        // body with no known content type. The caller refetches if needed.
        return Some(CacheRecord {
            content_type: None,
            body: data.to_vec(),
        });
    }
    if data.len() < 6 {
        return None;
    }
    let ct_len = u16::from_be_bytes([data[4], data[5]]) as usize;
    let ct_start = 6usize;
    let body_len_start = ct_start.checked_add(ct_len)?;
    let body_len_end = body_len_start.checked_add(8)?;
    if body_len_end > data.len() {
        return None;
    }
    let body_len_bytes: [u8; 8] =
        data[body_len_start..body_len_end].try_into().ok()?;
    let body_len = usize::try_from(u64::from_be_bytes(body_len_bytes)).ok()?;
    let body_start = body_len_end;
    let body_end = body_start.checked_add(body_len)?;
    if body_end > data.len() {
        return None;
    }
    let content_type = if ct_len == 0 {
        None
    } else {
        Some(String::from_utf8(data[ct_start..body_len_start].to_vec()).ok()?)
    };
    Some(CacheRecord {
        content_type,
        body: data[body_start..body_end].to_vec(),
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
    let record = match decode_record(&data) {
        Some(record) => record,
        None => {
            // Corrupt cache data is disposable. Remove it immediately so the
            // caller can refetch instead of repeatedly encountering the same
            // invalid record.
            remove(key).await;
            return None;
        }
    };
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
    Some(record)
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
    let encoded = encode_record(content_type, data)?;
    commit_record(&path, &encoded, &CURRENT_SIZE).await?;
    ensure_capacity().await;
    Ok(())
}

async fn commit_record(
    path: &Path,
    encoded: &[u8],
    tracked_size: &AtomicU64,
) -> std::io::Result<()> {
    let counter = WRITE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp =
        path.with_extension(format!("tmp.{}.{}", std::process::id(), counter));
    if let Err(error) = tokio::fs::write(&tmp, &encoded).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(error);
    }

    let _commit_guard = COMMIT_LOCK.lock().await;
    // Read the old size only immediately before the atomic replacement, while
    // holding the commit lock. Crucially, do not change CURRENT_SIZE until the
    // rename has succeeded: a failed overwrite leaves both the old entry and
    // its accounting intact.
    let old_len = tokio::fs::metadata(&path).await.ok().map(|meta| meta.len());
    if let Err(error) = tokio::fs::rename(&tmp, &path).await {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(error);
    }
    let new_len = u64::try_from(encoded.len()).unwrap_or(u64::MAX);
    let _ = tracked_size.fetch_update(
        Ordering::Relaxed,
        Ordering::Relaxed,
        |size| {
            Some(
                size.saturating_sub(old_len.unwrap_or(0))
                    .saturating_add(new_len),
            )
        },
    );
    drop(_commit_guard);
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

fn is_temp_file(entry: &walkdir::DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|name| name.ends_with(".tmp") || name.contains(".tmp."))
        .unwrap_or(false)
}

/// Skip stale temporary files left by interrupted atomic writes.
fn is_cache_file(entry: &walkdir::DirEntry) -> bool {
    !is_temp_file(entry)
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
        if tokio::fs::remove_file(&path).await.is_ok() {
            let _ = CURRENT_SIZE.fetch_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |size| Some(size.saturating_sub(len)),
            );
        }
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
            if entry.file_type().is_file() {
                if is_temp_file(&entry) {
                    // Interrupted writes are never valid cache entries. Best
                    // effort cleanup also prevents them accumulating forever.
                    let _ = std::fs::remove_file(entry.path());
                } else if is_cache_file(&entry) {
                    if let Ok(m) = entry.metadata() {
                        total = total.saturating_add(m.len());
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleanup_target_is_proportional_for_small_caches() {
        let gib = 1024u64 * 1024 * 1024;
        assert_eq!(clean_threshold(gib), gib * 85 / 100);
        assert_eq!(clean_threshold(4 * gib), 4 * gib * 85 / 100);
        assert!(clean_threshold(gib) > 0);
    }

    #[test]
    fn record_round_trip_preserves_body_and_content_type() {
        let encoded = encode_record(Some("image/webp"), b"payload").unwrap();
        let decoded = decode_record(&encoded).unwrap();
        assert_eq!(decoded.content_type.as_deref(), Some("image/webp"));
        assert_eq!(decoded.body, b"payload");
    }

    #[test]
    fn corrupt_content_type_length_is_rejected() {
        let mut data = CACHE_MAGIC.to_vec();
        data.extend_from_slice(&u16::MAX.to_be_bytes());
        data.extend_from_slice(b"short");
        assert!(decode_record(&data).is_none());
    }

    #[test]
    fn truncated_body_length_is_rejected() {
        let mut data = CACHE_MAGIC.to_vec();
        data.extend_from_slice(&0u16.to_be_bytes());
        data.extend_from_slice(&[0u8; 7]);
        assert!(decode_record(&data).is_none());
    }

    #[test]
    fn body_length_beyond_file_size_is_rejected() {
        let mut data = CACHE_MAGIC.to_vec();
        data.extend_from_slice(&0u16.to_be_bytes());
        data.extend_from_slice(&100u64.to_be_bytes());
        data.extend_from_slice(b"tiny");
        assert!(decode_record(&data).is_none());
    }

    #[test]
    fn truncated_current_format_is_not_treated_as_legacy() {
        assert!(decode_record(CACHE_MAGIC).is_none());
        let legacy = decode_record(b"old raw payload").unwrap();
        assert_eq!(legacy.body, b"old raw payload");
    }

    #[test]
    fn stale_temp_names_are_not_cache_files() {
        let root = std::env::temp_dir().join(format!(
            "replex-disk-cache-entry-{}-{}",
            std::process::id(),
            WRITE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        let temp_path = root.join("abcdef.tmp.123.456");
        std::fs::write(&temp_path, b"partial").unwrap();
        let entry = walkdir::WalkDir::new(&root)
            .min_depth(1)
            .max_depth(1)
            .into_iter()
            .next()
            .unwrap()
            .unwrap();
        assert!(is_temp_file(&entry));
        assert!(!is_cache_file(&entry));
        let _ = std::fs::remove_dir_all(root);
    }

    fn test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "replex-disk-cache-{name}-{}-{}",
            std::process::id(),
            WRITE_COUNTER.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[tokio::test]
    async fn overwrite_accounting_tracks_larger_and_smaller_records() {
        let root = test_dir("overwrite");
        tokio::fs::create_dir_all(&root).await.unwrap();
        let path = root.join("entry");
        let tracked = AtomicU64::new(0);

        let first = encode_record(None, b"small").unwrap();
        commit_record(&path, &first, &tracked).await.unwrap();
        assert_eq!(tracked.load(Ordering::Relaxed), first.len() as u64);

        let larger = encode_record(None, &[42u8; 4096]).unwrap();
        commit_record(&path, &larger, &tracked).await.unwrap();
        assert_eq!(tracked.load(Ordering::Relaxed), larger.len() as u64);

        let smaller = encode_record(None, b"x").unwrap();
        commit_record(&path, &smaller, &tracked).await.unwrap();
        assert_eq!(tracked.load(Ordering::Relaxed), smaller.len() as u64);
        assert_eq!(tokio::fs::read(&path).await.unwrap(), smaller);

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn failed_commit_does_not_change_accounting() {
        let root = test_dir("failed-commit");
        tokio::fs::create_dir_all(&root).await.unwrap();
        // A directory at the destination makes rename fail deterministically.
        let path = root.join("entry");
        tokio::fs::create_dir_all(&path).await.unwrap();
        let tracked = AtomicU64::new(1234);
        let encoded = encode_record(None, b"payload").unwrap();

        assert!(commit_record(&path, &encoded, &tracked).await.is_err());
        assert_eq!(tracked.load(Ordering::Relaxed), 1234);

        let temp_prefix = "entry.tmp.";
        let mut entries = tokio::fs::read_dir(&root).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                !name.starts_with(temp_prefix),
                "temporary file leaked: {name}"
            );
        }

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn concurrent_writes_to_one_key_leave_one_valid_record() {
        let root = test_dir("concurrent");
        tokio::fs::create_dir_all(&root).await.unwrap();
        let path = root.join("entry");
        let tracked = AtomicU64::new(0);
        let first = encode_record(Some("text/plain"), b"first").unwrap();
        let second = encode_record(Some("text/plain"), b"second").unwrap();

        let (a, b) = tokio::join!(
            commit_record(&path, &first, &tracked),
            commit_record(&path, &second, &tracked)
        );
        a.unwrap();
        b.unwrap();

        let final_bytes = tokio::fs::read(&path).await.unwrap();
        let decoded = decode_record(&final_bytes).unwrap();
        assert!(decoded.body == b"first" || decoded.body == b"second");
        assert_eq!(tracked.load(Ordering::Relaxed), final_bytes.len() as u64);

        let mut entries = tokio::fs::read_dir(&root).await.unwrap();
        while let Some(entry) = entries.next_entry().await.unwrap() {
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(!name.contains(".tmp."), "temporary file leaked: {name}");
        }

        let _ = tokio::fs::remove_dir_all(root).await;
    }
}
