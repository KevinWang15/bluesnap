//! Atomic replacement of private project and settings files.

use std::io::{self, Write};
use std::path::Path;

/// Stages each write in its own file on the destination filesystem. On Unix the
/// staging file is private from creation, including when replacing an older file.
pub fn write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut file = tempfile::NamedTempFile::new_in(parent)?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    let staged = file.into_temp_path();
    #[cfg(windows)]
    {
        persist_on_windows(staged, path)
    }
    #[cfg(not(windows))]
    {
        Ok(staged.persist(path)?)
    }
}

// MoveFileEx can briefly deny replacement while another process is finishing a
// rename, even with all our own write handles closed. Retry only the rename.
#[cfg(windows)]
fn persist_on_windows(mut staged: tempfile::TempPath, path: &Path) -> io::Result<()> {
    for delay_ms in [2, 4, 8, 16, 32, 64] {
        match staged.persist(path) {
            Ok(()) => return Ok(()),
            // ERROR_ACCESS_DENIED, ERROR_SHARING_VIOLATION, ERROR_LOCK_VIOLATION.
            Err(e) if matches!(e.error.raw_os_error(), Some(5 | 32 | 33)) => {
                staged = e.path;
                std::thread::sleep(std::time::Duration::from_millis(delay_ms));
            }
            Err(e) => return Err(e.error),
        }
    }
    Ok(staged.persist(path)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn concurrent_replacements_each_publish_one_complete_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("project");
        let ready = std::sync::Barrier::new(8);
        std::thread::scope(|threads| {
            for byte in 0..8 {
                let (path, ready) = (&path, &ready);
                threads.spawn(move || {
                    let bytes = vec![byte; 256 * 1024];
                    ready.wait();
                    write(path, &bytes).unwrap();
                });
            }
        });
        let bytes = std::fs::read(path).unwrap();
        assert_eq!(bytes.len(), 256 * 1024);
        assert!(bytes.iter().all(|&b| b == bytes[0]));
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
