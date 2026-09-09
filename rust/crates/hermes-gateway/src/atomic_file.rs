//! Shared atomic replacement for registry and endpoint cache files.
use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

pub(crate) fn write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_to(path, bytes, false)
}

/// Atomically replace a user-owned config file without detaching a symlink.
/// The temporary file is private before publication, and the published target
/// is owner-readable/writable only. This is the native counterpart to
/// `utils.atomic_roundtrip_yaml_update`'s filesystem contract.
pub(crate) fn write_private_preserving_symlink(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let target = resolve_symlink_target(path)?;
    write_to(&target, bytes, true)
}

/// Atomically replace a private cache or mirror file. Unlike user-owned
/// configuration, a symlink at this path is detached instead of followed.
pub(crate) fn write_private_replacing_symlink(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    write_to(path, bytes, true)
}

fn resolve_symlink_target(path: &Path) -> std::io::Result<PathBuf> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            let target = std::fs::read_link(path)?;
            let target = if target.is_absolute() {
                target
            } else {
                path.parent().unwrap_or_else(|| Path::new(".")).join(target)
            };
            Ok(target.canonicalize().unwrap_or(target))
        }
        Ok(_) => Ok(path.to_path_buf()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(path.to_path_buf()),
        Err(e) => Err(e),
    }
}

fn write_to(path: &Path, bytes: &[u8], private: bool) -> std::io::Result<()> {
    static SEQUENCE: AtomicU64 = AtomicU64::new(0);
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(
        ".hermes-cache-{}-{}.tmp",
        std::process::id(),
        SEQUENCE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut created = false;
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        created = true;
        preserve_owner(path, &file);
        file.write_all(bytes)?;
        file.sync_all()?;
        if private {
            set_private(&file)?;
        } else if let Ok(metadata) = std::fs::metadata(path) {
            file.set_permissions(metadata.permissions())?;
        }
        std::fs::rename(&temporary, path)?;
        sync_parent(parent);
        Ok(())
    })();
    if result.is_err() && created {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(unix)]
fn preserve_owner(path: &Path, file: &File) {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::MetadataExt;

    let Ok(metadata) = std::fs::metadata(path) else {
        return;
    };
    // Best effort, matching Python's owner restoration. An unprivileged owner
    // may not be allowed to chown even to the existing ids on every platform.
    unsafe {
        libc::fchown(
            file.as_raw_fd(),
            metadata.uid() as libc::uid_t,
            metadata.gid() as libc::gid_t,
        );
    }
}

#[cfg(not(unix))]
fn preserve_owner(_path: &Path, _file: &File) {}

#[cfg(unix)]
fn set_private(file: &File) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn set_private(_file: &File) -> std::io::Result<()> {
    Ok(())
}

fn sync_parent(parent: &Path) {
    if let Ok(directory) = File::open(parent) {
        let _ = directory.sync_all();
    }
}
