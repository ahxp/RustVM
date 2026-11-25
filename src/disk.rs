use anyhow::{Context, Result, bail};
use std::fs::{self, OpenOptions};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::Path;

const GIB: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone, Copy, Default)]
pub struct DiskUsage {
    #[allow(dead_code)]
    pub logical_bytes: u64,
    pub actual_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct CompactStats {
    pub before_bytes: u64,
    pub after_bytes: u64,
    pub reclaimed_bytes: u64,
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

/// Compacts a stopped VM's raw disk image in-place on APFS by scanning with `F_NOCACHE`
/// (avoiding host RAM cache pollution) and punching sparse holes (`F_PUNCHHOLE`) over
/// 16 KiB-aligned zeroed extents.
pub fn compact_sparse_disk(path: &Path) -> Result<CompactStats> {
    if !path.is_file() {
        bail!("Disk image not found: {}", path.display());
    }
    let before = disk_usage(path).actual_bytes;

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .with_context(|| format!("Failed to open disk image {}", path.display()))?;
    let total_len = file.metadata()?.len();
    let fd = file.as_raw_fd();

    // Bypass macOS unified page cache so scanning a multi-GB disk doesn't evict active apps
    unsafe {
        let _ = libc::fcntl(fd, libc::F_NOCACHE, 1);
    }

    const CHUNK_SIZE: usize = 4 * 1024 * 1024; // 4 MiB read window
    const BLOCK_ALIGN: usize = 16 * 1024; // 16 KiB alignment satisfies 4K and 16K APFS blocks
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut offset: u64 = 0;

    while offset < total_len {
        let to_read = ((total_len - offset) as usize).min(CHUNK_SIZE);
        let n = file
            .read_at(&mut buf[..to_read], offset)
            .with_context(|| format!("Failed reading disk image at offset {offset}"))?;
        if n == 0 {
            break;
        }

        let aligned_len = (n / BLOCK_ALIGN) * BLOCK_ALIGN;
        let mut run_start: Option<usize> = None;

        let mut pos = 0;
        while pos < aligned_len {
            let is_zero = is_all_zero(&buf[pos..pos + BLOCK_ALIGN]);
            match (is_zero, run_start) {
                (true, None) => run_start = Some(pos),
                (false, Some(start)) => {
                    punch_hole(fd, offset + start as u64, (pos - start) as u64);
                    run_start = None;
                }
                _ => {}
            }
            pos += BLOCK_ALIGN;
        }
        if let Some(start) = run_start {
            punch_hole(fd, offset + start as u64, (aligned_len - start) as u64);
        }

        offset += n as u64;
    }

    let _ = file.sync_all();
    let after = disk_usage(path).actual_bytes;
    Ok(CompactStats {
        before_bytes: before,
        after_bytes: after,
        reclaimed_bytes: before.saturating_sub(after),
    })
}

fn punch_hole(fd: i32, offset: u64, length: u64) {
    if length == 0 {
        return;
    }
    let mut req = libc::fpunchhole_t {
        fp_flags: 0,
        reserved: 0,
        fp_offset: offset as libc::off_t,
        fp_length: length as libc::off_t,
    };
    unsafe {
        let _ = libc::fcntl(fd, libc::F_PUNCHHOLE, &mut req);
    }
}

fn is_all_zero(slice: &[u8]) -> bool {
    let (prefix, words, suffix) = unsafe { slice.align_to::<u128>() };
    prefix.iter().all(|&b| b == 0)
        && words.iter().all(|&w| w == 0)
        && suffix.iter().all(|&b| b == 0)
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

/// Returns available free disk space in bytes on the filesystem containing `path`.
pub fn available_space_bytes(path: &Path) -> Option<u64> {
    let check_path = if path.exists() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    let mut c_path = check_path.as_os_str().as_bytes().to_vec();
    c_path.push(0);
    let mut vfs: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr() as *const libc::c_char, &mut vfs) };
    if rc == 0 {
        let block_size = if vfs.f_frsize > 0 {
            vfs.f_frsize as u64
        } else {
            vfs.f_bsize as u64
        };
        Some((vfs.f_bavail as u64).saturating_mul(block_size))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Seek, SeekFrom, Write};

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

    #[test]
    fn compact_sparse_disk_punches_zero_blocks_on_apfs() {
        let path =
            std::env::temp_dir().join(format!("rustvm_compact_{}.img", uuid::Uuid::new_v4()));
        create_sparse_disk(&path, 1).unwrap();

        // Materialize 8 MiB of physical zero blocks + 64 KiB of non-zero data
        {
            let mut f = OpenOptions::new().write(true).open(&path).unwrap();
            let zeros = vec![0u8; 8 * 1024 * 1024];
            f.write_all(&zeros).unwrap();
            let data = vec![0xAAu8; 64 * 1024];
            f.seek(SeekFrom::Start(16 * 1024 * 1024)).unwrap();
            f.write_all(&data).unwrap();
            f.sync_all().unwrap();
        }

        let stats = compact_sparse_disk(&path).unwrap();
        assert!(stats.after_bytes <= stats.before_bytes);
        assert!(stats.after_bytes < 2 * 1024 * 1024);

        // Verify non-zero payload at 16 MiB is completely intact
        let f = OpenOptions::new().read(true).open(&path).unwrap();
        let mut verify = vec![0u8; 64 * 1024];
        f.read_at(&mut verify, 16 * 1024 * 1024).unwrap();
        assert!(verify.iter().all(|&b| b == 0xAA));

        let _ = fs::remove_file(&path);
    }
}
