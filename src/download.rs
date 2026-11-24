use anyhow::{Context, Result, bail};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::catalog::{OsPreset, preset_iso_path};
use crate::config::VmStore;

#[derive(Debug, Clone, Copy)]
pub struct DownloadProgress {
    pub downloaded_bytes: u64,
    pub total_bytes: Option<u64>,
    pub bytes_per_sec: f64,
}

pub fn download_preset<F>(
    store: &VmStore,
    preset: &OsPreset,
    cancel: Arc<AtomicBool>,
    on_progress: F,
) -> Result<PathBuf>
where
    F: FnMut(DownloadProgress),
{
    store.ensure_dirs()?;
    let dest = preset_iso_path(store, preset);
    if dest.is_file() && dest.metadata().map(|m| m.len() > 1024 * 1024).unwrap_or(false) {
        return Ok(dest);
    }
    download_file(preset.url, &dest, cancel, on_progress)?;
    Ok(dest)
}

pub fn download_file<F>(
    url: &str,
    dest: &Path,
    cancel: Arc<AtomicBool>,
    mut on_progress: F,
) -> Result<()>
where
    F: FnMut(DownloadProgress),
{
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent)?;
    }

    let tmp_path = dest.with_extension("iso.part");
    let _ = fs::remove_file(&tmp_path);

    let agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_body(None)
        .user_agent("RustVM/1.1.1")
        .build()
        .new_agent();

    let mut response = agent
        .get(url)
        .call()
        .with_context(|| format!("Failed to download {url}"))?;

    let total_bytes = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let mut reader = response.body_mut().as_reader();
    let mut file = File::create(&tmp_path)
        .with_context(|| format!("Failed to create temp file {}", tmp_path.display()))?;

    let mut buf = vec![0u8; 256 * 1024];
    let mut downloaded: u64 = 0;
    let started = Instant::now();
    let mut last_report = Instant::now();

    on_progress(DownloadProgress {
        downloaded_bytes: 0,
        total_bytes,
        bytes_per_sec: 0.0,
    });

    loop {
        if cancel.load(Ordering::Relaxed) {
            drop(file);
            let _ = fs::remove_file(&tmp_path);
            bail!("Download cancelled");
        }
        let n = reader.read(&mut buf).context("Error reading download stream")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .context("Error writing downloaded ISO to disk")?;
        downloaded += n as u64;

        if last_report.elapsed() >= Duration::from_millis(120) {
            let elapsed = started.elapsed().as_secs_f64().max(0.001);
            on_progress(DownloadProgress {
                downloaded_bytes: downloaded,
                total_bytes,
                bytes_per_sec: downloaded as f64 / elapsed,
            });
            last_report = Instant::now();
        }
    }

    file.flush()?;
    drop(file);

    if let Some(expected) = total_bytes {
        if downloaded < expected {
            let _ = fs::remove_file(&tmp_path);
            bail!(
                "Incomplete download: received {downloaded} bytes of {expected} expected"
            );
        }
    }

    fs::rename(&tmp_path, dest).with_context(|| {
        format!(
            "Failed to move downloaded file {} to {}",
            tmp_path.display(),
            dest.display()
        )
    })?;

    let elapsed = started.elapsed().as_secs_f64().max(0.001);
    on_progress(DownloadProgress {
        downloaded_bytes: downloaded,
        total_bytes: total_bytes.or(Some(downloaded)),
        bytes_per_sec: downloaded as f64 / elapsed,
    });

    Ok(())
}
