use anyhow::{Context, Result, bail};
use std::fs::{self, File, OpenOptions};
use std::io::{Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::path::Path;

/// Holds an exclusive non-blocking POSIX advisory lock (`flock(LOCK_EX | LOCK_NB)`)
/// on `~/RustVM/vms/<id>/vm.lock` for the entire lifetime of a running VM.
///
/// If the RustVM process crashes or is killed (`SIGKILL`), the macOS kernel
/// automatically releases the lock so the VM is never permanently wedged.
pub struct VmLock {
    file: File,
}

impl VmLock {
    pub fn acquire(lock_path: &Path, vm_name: &str) -> Result<Self> {
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .with_context(|| format!("Failed to open VM lockfile {}", lock_path.display()))?;

        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let pid_hint = read_pid_from_path(lock_path)
                .filter(|&p| p > 0)
                .map(|p| format!(" (PID {p})"))
                .unwrap_or_default();
            bail!(
                "Virtual machine '{vm_name}' is already running{pid_hint}.\n\
                 Focus its existing window or shut it down before starting or modifying it again."
            );
        }

        let _ = file.set_len(0);
        let _ = file.seek(SeekFrom::Start(0));
        let _ = writeln!(file, "{}", std::process::id());
        let _ = file.flush();

        Ok(Self { file })
    }
}

impl Drop for VmLock {
    fn drop(&mut self) {
        let _ = self.file.set_len(0);
        unsafe {
            let _ = libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

/// Checks whether a VM is currently locked by an active `VmRunner` process.
/// Returns `Some(pid)` (or `Some(0)` if PID unknown) when locked, or `None` when stopped.
pub fn is_vm_locked(lock_path: &Path) -> Option<u32> {
    if !lock_path.exists() {
        return None;
    }
    let Ok(file) = OpenOptions::new().read(true).open(lock_path) else {
        return None;
    };
    let fd = file.as_raw_fd();
    let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        unsafe {
            let _ = libc::flock(fd, libc::LOCK_UN);
        }
        None
    } else {
        Some(read_pid_from_path(lock_path).unwrap_or(0))
    }
}

fn read_pid_from_path(lock_path: &Path) -> Option<u32> {
    fs::read_to_string(lock_path)
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exclusive_vm_lock_blocks_second_acquire_and_releases_on_drop() {
        let path = std::env::temp_dir().join(format!("rustvm_lock_{}.lock", uuid::Uuid::new_v4()));
        assert!(is_vm_locked(&path).is_none());

        {
            let _guard = VmLock::acquire(&path, "TestVM").unwrap();
            assert_eq!(is_vm_locked(&path), Some(std::process::id()));
            assert!(VmLock::acquire(&path, "TestVM").is_err());
        }

        assert!(is_vm_locked(&path).is_none());
        let _guard2 = VmLock::acquire(&path, "TestVM").unwrap();
        drop(_guard2);
        let _ = fs::remove_file(&path);
    }
}
