use anyhow::{Context, Result, bail};
use std::fs::{self, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default)]
pub struct DiskUsage {
    #[allow(dead_code)]
    pub logical_bytes: u64,
    pub actual_bytes: u64,
}

/// Creates a sparse raw disk image of `size_gb` gigabytes.
/// On APFS this is instantaneous and consumes almost zero physical space until written.
pub fn create_sparse_disk(path: &Path, size_gb: u64) -> Result<()> {
    if size_gb < 1 {
        bail!("Disk size must be at least 1 GB");
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("Failed to create disk image {}", path.display()))?;
    file.set_len(size_gb * GIB)
        .with_context(|| format!("Failed to size disk image to {size_gb} GB"))?;
    Ok(())
}

/// Grows an existing sparse disk image to `new_size_gb`. Shrinking is refused to prevent data loss.
pub fn grow_sparse_disk(path: &Path, new_size_gb: u64) -> Result<()> {
    let meta = fs::metadata(path)
        .with_context(|| format!("Disk image not found: {}", path.display()))?;
    let target_bytes = new_size_gb * GIB;
    if target_bytes < meta.len() {
        bail!(
            "Cannot shrink disk image from {} GB to {} GB (would corrupt guest filesystem)",
            meta.len() / GIB,
            new_size_gb
        );
    }
    let file = OpenOptions::new()
        .write(true)
        .open(path)
        .with_context(|| format!("Failed to open disk image {}", path.display()))?;
    file.set_len(target_bytes)
        .with_context(|| format!("Failed to resize disk image to {new_size_gb} GB"))?;
    Ok(())
}

/// Returns both logical (allocated) size and actual physical disk usage (`st_blocks * 512`).
pub fn disk_usage(path: &Path) -> DiskUsage {
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    bytes.push(0);
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::stat(bytes.as_ptr() as *const libc::c_char, &mut st) };
    if rc == 0 {
        DiskUsage {
            logical_bytes: st.st_size.max(0) as u64,
            actual_bytes: (st.st_blocks.max(0) as u64) * 512,
        }
    } else {
        DiskUsage::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sparse_disk_create_and_grow_refuses_shrink() {
        let path = std::env::temp_dir().join(format!("rustvm_disk_{}.img", uuid::Uuid::new_v4()));
        create_sparse_disk(&path, 2).unwrap();
        let u = disk_usage(&path);
        assert_eq!(u.logical_bytes, 2 * GIB);
        assert!(u.actual_bytes < 10 * 1024 * 1024);

        grow_sparse_disk(&path, 4).unwrap();
        assert_eq!(disk_usage(&path).logical_bytes, 4 * GIB);
        assert!(grow_sparse_disk(&path, 1).is_err());

        let _ = fs::remove_file(&path);
    }
}
