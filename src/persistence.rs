//! Атомарная замена файлов состояния в том же каталоге.
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

pub fn atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    atomic_write_with(path, |file| file.write_all(bytes))
}

pub fn atomic_write_with(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("нет каталога файла"))?;
    std::fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .ok_or_else(|| std::io::Error::other("нет имени файла"))?;
    let temp = parent.join(format!(
        ".{}.{}-{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temp)?;
    let result = (|| {
        write(&mut file)?;
        file.sync_all()?;
        std::fs::rename(&temp, path)?;
        std::fs::File::open(parent)?.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// JSONL stays chronological on disk; retain only the requested tail in memory.
pub fn recent_json<T: serde::de::DeserializeOwned>(
    reader: impl std::io::BufRead,
    limit: usize,
) -> Vec<T> {
    let mut entries = std::collections::VecDeque::new();
    if limit == 0 {
        return Vec::new();
    }
    for line in reader.lines().map_while(Result::ok) {
        if let Ok(entry) = serde_json::from_str::<T>(&line) {
            if entries.len() == limit {
                entries.pop_front();
            }
            entries.push_back(entry);
        }
    }
    entries.into_iter().rev().collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn recent_json_keeps_valid_tail_newest_first() {
        let input = b"1\n2\ninvalid\n3\n4\n";
        assert_eq!(super::recent_json::<u32>(&input[..], 2), [4, 3]);
        assert!(super::recent_json::<u32>(&input[..], 0).is_empty());
    }
}
