//! Port of hermes_cli/install_identity.py.
//!
// Public API is ahead of some callers (hosted-room authority uses it).
#![allow(dead_code)]
//!
//! Stable opaque identity shared by every profile in one Hermes install: a
//! 32-hex id persisted at `<hermes_root>/install_id`. `read_or_create_install_id`
//! reads it, or atomically mints one under a cross-process file lock. `None`
//! means the id could neither be read nor persisted (an ephemeral id would
//! violate the room-authority / connection-registry contract, so callers fail
//! closed). The id is minted from the kernel CSPRNG (it gates room authority),
//! matching Python's `uuid4().hex` as an opaque random 32-hex value.

use std::path::Path;
use std::sync::Mutex;

const INSTALL_ID_FILENAME: &str = "install_id";

/// True when `s` is exactly 32 lowercase hex chars.
fn is_valid_id(s: &str) -> bool {
    s.len() == 32
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn read_valid(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let candidate = text.trim().to_lowercase();
    if is_valid_id(&candidate) {
        Some(candidate)
    } else {
        None
    }
}

/// Fill `buf` from the kernel CSPRNG (`/dev/urandom`, then `getrandom(2)`).
fn fill_random(buf: &mut [u8]) -> bool {
    use std::io::Read;
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        if f.read_exact(buf).is_ok() {
            return true;
        }
    }
    #[cfg(target_os = "linux")]
    {
        let mut filled = 0usize;
        while filled < buf.len() {
            // SAFETY: writing into our own buffer for the remaining length.
            let rc = unsafe {
                libc::getrandom(
                    buf[filled..].as_mut_ptr() as *mut libc::c_void,
                    buf.len() - filled,
                    0,
                )
            };
            if rc > 0 {
                filled += rc as usize;
            } else if rc == 0 {
                break;
            } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
                continue;
            } else {
                break;
            }
        }
        return filled == buf.len();
    }
    #[allow(unreachable_code)]
    false
}

/// A random 32-hex id from 16 CSPRNG bytes (the shape of `uuid4().hex`).
pub(crate) fn mint_id() -> Option<String> {
    let mut bytes = [0u8; 16];
    if !fill_random(&mut bytes) {
        return None;
    }
    Some(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Hold an exclusive advisory lock on `<root>/.install_id.lock` for the
/// publication fence. The lock is released when the returned guard drops.
#[cfg(unix)]
struct FileLock {
    _file: std::fs::File,
}

#[cfg(unix)]
fn acquire_file_lock(root: &Path) -> Option<FileLock> {
    use std::os::unix::io::AsRawFd;
    let lock_path = root.join(".install_id.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(&lock_path)
        .ok()?;
    // SAFETY: flock on a valid open fd; blocks until the lock is available.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        return None;
    }
    Some(FileLock { _file: file })
}

#[cfg(unix)]
impl Drop for FileLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // Best-effort unlock; closing the fd (on drop) also releases it.
        unsafe { libc::flock(self._file.as_raw_fd(), libc::LOCK_UN) };
    }
}

fn write_atomic(root: &Path, path: &Path, minted: &str) -> bool {
    let tmp = root.join(format!(".install_id-{}", std::process::id()));
    if std::fs::write(&tmp, format!("{minted}\n")).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    // Best-effort directory fsync for durability of the rename.
    #[cfg(unix)]
    if let Ok(f) = std::fs::File::open(root) {
        let _ = f.sync_all();
    }
    true
}

/// Read the install id, or atomically mint one. `None` when it could neither be
/// read nor persisted.
pub fn read_or_create_install_id(root: Option<&Path>) -> Option<String> {
    let root_buf;
    let root = match root {
        Some(r) => r,
        None => {
            root_buf = crate::config_file::hermes_root();
            &root_buf
        }
    };
    let path = root.join(INSTALL_ID_FILENAME);

    // Fast path: an existing valid id.
    if let Some(id) = read_valid(&path) {
        return Some(id);
    }

    if std::fs::create_dir_all(root).is_err() {
        return None;
    }

    // Publication fence: re-check under the lock, then mint + persist.
    let _lock = acquire_file_lock(root)?;
    if let Some(id) = read_valid(&path) {
        return Some(id);
    }
    let minted = mint_id()?;
    if !write_atomic(root, &path, &minted) {
        return None;
    }
    read_valid(&path)
}

struct Cache {
    root: Option<String>,
    value: Option<String>,
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: Mutex<Cache> = Mutex::new(Cache {
        root: None,
        value: None,
    });
    &CACHE
}

/// The process-cached stable id for the active Hermes root, or `None`.
pub fn get_install_id() -> Option<String> {
    let root = crate::config_file::hermes_root();
    let root_key = root.to_string_lossy().to_string();

    {
        let c = cache().lock().unwrap();
        if let Some(v) = &c.value {
            if c.root.is_none() || c.root.as_deref() == Some(root_key.as_str()) {
                return Some(v.clone());
            }
        }
    }

    let mut c = cache().lock().unwrap();
    if let Some(v) = &c.value {
        if c.root.is_none() || c.root.as_deref() == Some(root_key.as_str()) {
            return Some(v.clone());
        }
    }
    let value = read_or_create_install_id(Some(&root));
    if let Some(v) = &value {
        c.root = Some(root_key);
        c.value = Some(v.clone());
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "hermes_installid_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn mints_and_persists_a_valid_id() {
        let root = temp_root("mint");
        let id = read_or_create_install_id(Some(&root)).unwrap();
        assert!(is_valid_id(&id), "minted id is 32 lowercase hex: {id}");
        // The file now holds it and a second read returns the same value.
        assert_eq!(
            read_or_create_install_id(Some(&root)).as_deref(),
            Some(id.as_str())
        );
        let on_disk = std::fs::read_to_string(root.join("install_id")).unwrap();
        assert_eq!(on_disk.trim(), id);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reads_an_existing_valid_id() {
        let root = temp_root("read");
        std::fs::write(
            root.join("install_id"),
            "0123456789ABCDEF0123456789abcdef\n",
        )
        .unwrap();
        // Case-normalized to lowercase on read.
        assert_eq!(
            read_or_create_install_id(Some(&root)).as_deref(),
            Some("0123456789abcdef0123456789abcdef")
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rejects_a_malformed_file_and_remints() {
        let root = temp_root("bad");
        std::fs::write(root.join("install_id"), "not-a-valid-id\n").unwrap();
        let id = read_or_create_install_id(Some(&root)).unwrap();
        assert!(is_valid_id(&id));
        assert_ne!(id, "not-a-valid-id");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn is_valid_id_shape() {
        assert!(is_valid_id("0123456789abcdef0123456789abcdef"));
        assert!(!is_valid_id("0123456789ABCDEF0123456789abcdef")); // uppercase
        assert!(!is_valid_id("short"));
        assert!(!is_valid_id("g123456789abcdef0123456789abcdef")); // non-hex
    }
}
