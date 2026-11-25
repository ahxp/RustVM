use anyhow::{Context, Result, bail};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::catalog::{OsPreset, preset_iso_path};
use crate::config::{VmStore, format_bytes};
use crate::disk;

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
    let resume_from = fs::metadata(&tmp_path).map(|m| m.len()).unwrap_or(0);

    // 30-second per-read timeout prevents infinite hangs if Wi-Fi drops mid-download
    let agent = ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(20)))
        .timeout_recv_response(Some(Duration::from_secs(30)))
        .timeout_per_read(Some(Duration::from_secs(30)))
        .http_status_as_error(false)
        .user_agent("RustVM/1.2.1")
        .build()
        .new_agent();

    let mut req = agent.get(url);
    if resume_from > 0 {
        req = req.header("Range", &format!("bytes={resume_from}-"));
    }

    let mut response = req
        .call()
        .with_context(|| format!("Failed to connect to {url}"))?;

    let mut status = response.status().as_u16();
    if status == 416 && resume_from > 0 {
        // Stale or complete .iso.part — restart fresh
        let _ = fs::remove_file(&tmp_path);
        response = agent
            .get(url)
            .call()
            .with_context(|| format!("Failed to download {url}"))?;
        status = response.status().as_u16();
    }

    if status >= 400 {
        bail!("HTTP {status} when downloading {url}");
    }

    let content_len = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<u64>().ok());

    let (mut file, mut downloaded, total_bytes) = if status == 206 && resume_from > 0 {
        let f = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&tmp_path)
            .with_context(|| format!("Failed to open partial download {}", tmp_path.display()))?;
        (f, resume_from, content_len.map(|rem| resume_from + rem))
    } else {
        let f = File::create(&tmp_path)
            .with_context(|| format!("Failed to create temp file {}", tmp_path.display()))?;
        (f, 0u64, content_len)
    };

    // Preflight host disk space check for remaining bytes
    if let (Some(total), Some(avail)) = (total_bytes, disk::available_space_bytes(&tmp_path)) {
        let remaining = total.saturating_sub(downloaded);
        if avail < remaining.saturating_add(128 * 1024 * 1024) {
            bail!(
                "Not enough free disk space to download ISO ({} free, {} needed).",
                format_bytes(avail),
                format_bytes(remaining)
            );
        }
    }

    let mut reader = response.body_mut().as_reader();
    let mut buf = vec![0u8; 256 * 1024];
    let session_start_bytes = downloaded;
    let started = Instant::now();
    let mut last_report = Instant::now();

    on_progress(DownloadProgress {
        downloaded_bytes: downloaded,
        total_bytes,
        bytes_per_sec: 0.0,
    });

    loop {
        if cancel.load(Ordering::Relaxed) {
            let _ = file.flush();
            bail!("Download cancelled (partial progress saved — click Download again to resume)");
        }
        let n = reader
            .read(&mut buf)
            .context("Download stream interrupted or timed out — click Download again to resume")?;
        if n == 0 {
            break;
        }
        file.write_all(&buf[..n])
            .context("Error writing downloaded ISO to disk")?;
        downloaded += n as u64;

        if last_report.elapsed() >= Duration::from_millis(120) {
            let elapsed = started.elapsed().as_secs_f64().max(0.001);
            let session_bytes = downloaded.saturating_sub(session_start_bytes);
            on_progress(DownloadProgress {
                downloaded_bytes: downloaded,
                total_bytes,
                bytes_per_sec: session_bytes as f64 / elapsed,
            });
            last_report = Instant::now();
        }
    }

    file.flush()?;
    drop(file);

    if let Some(expected) = total_bytes {
        if downloaded < expected {
            bail!(
                "Incomplete download ({} of {} received) — click Download again to resume",
                format_bytes(downloaded),
                format_bytes(expected)
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
    let session_bytes = downloaded.saturating_sub(session_start_bytes);
    on_progress(DownloadProgress {
        downloaded_bytes: downloaded,
        total_bytes: total_bytes.or(Some(downloaded)),
        bytes_per_sec: session_bytes as f64 / elapsed,
    });

    Ok(())
}
