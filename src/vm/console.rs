use anyhow::{Context, Result};
use objc2::AnyThread;
use objc2::rc::Retained;
use objc2_foundation::NSFileHandle;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};

pub struct SerialBridge {
    guest_read_handle: Retained<NSFileHandle>,
    guest_write_handle: Retained<NSFileHandle>,
    stop_flag: Arc<AtomicBool>,
    host_write_fd: Option<RawFd>,
    threads: Vec<JoinHandle<()>>,
    saved_termios: Option<libc::termios>,
}

impl SerialBridge {
    /// Creates a serial bridge for GUI VMs: guest output is written to `serial.log`,
    /// and guest input reads from `/dev/null` (never blocks).
    pub fn new_log_only(log_path: &Path) -> Result<Self> {
        let dev_null = OpenOptions::new()
            .read(true)
            .open("/dev/null")
            .context("Failed to open /dev/null")?;

        let log_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(log_path)
            .with_context(|| format!("Failed to open serial log {}", log_path.display()))?;

        let guest_read_handle = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
            NSFileHandle::alloc(),
            dev_null.into_raw_fd(),
            true,
        );
        let guest_write_handle = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
            NSFileHandle::alloc(),
            log_file.into_raw_fd(),
            true,
        );

        Ok(Self {
            guest_read_handle,
            guest_write_handle,
            stop_flag: Arc::new(AtomicBool::new(false)),
            host_write_fd: None,
            threads: Vec::new(),
            saved_termios: None,
        })
    }

    /// Creates an interactive headless serial console bridged to the current terminal
    /// (also tee'd to `serial.log`). Pressing `Ctrl+]` triggers `on_escape`.
    pub fn new_interactive<F>(log_path: &Path, on_escape: F) -> Result<Self>
    where
        F: Fn() + Send + 'static,
    {
        let (in_read_fd, in_write_fd) = create_pipe()?;
        let (out_read_fd, out_write_fd) = create_pipe()?;

        let guest_read_handle = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
            NSFileHandle::alloc(),
            in_read_fd,
            true,
        );
        let guest_write_handle = NSFileHandle::initWithFileDescriptor_closeOnDealloc(
            NSFileHandle::alloc(),
            out_write_fd,
            true,
        );

        let saved_termios = enable_raw_stdin();
        let stop_flag = Arc::new(AtomicBool::new(false));
        let mut threads = Vec::new();

        // Thread 1: Guest serial output -> host stdout + serial.log
        let log_file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(log_path)
            .with_context(|| format!("Failed to open serial log {}", log_path.display()))?;
        let stop_out = Arc::clone(&stop_flag);
        threads.push(thread::spawn(move || {
            let mut reader = unsafe { File::from_raw_fd(out_read_fd) };
            let mut log = log_file;
            let mut stdout = std::io::stdout();
            let mut buf = [0u8; 4096];
            while !stop_out.load(Ordering::Relaxed) {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let _ = stdout.write_all(&buf[..n]);
                        let _ = stdout.flush();
                        let _ = log.write_all(&buf[..n]);
                        let _ = log.flush();
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        }));

        // Thread 2: Host stdin -> guest serial input (with Ctrl+] escape)
        let stop_in = Arc::clone(&stop_flag);
        let stdin_is_tty = saved_termios.is_some();
        threads.push(thread::spawn(move || {
            let mut stdin = std::io::stdin();
            let mut buf = [0u8; 256];
            while !stop_in.load(Ordering::Relaxed) {
                // Poll stdin with a short timeout so we can exit cleanly when VM stops
                let mut pfd = libc::pollfd {
                    fd: libc::STDIN_FILENO,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut pfd, 1, 150) };
                if ready <= 0 {
                    continue;
                }
                if (pfd.revents & (libc::POLLHUP | libc::POLLERR)) != 0 && (pfd.revents & libc::POLLIN) == 0 {
                    break;
                }
                match stdin.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        let slice = &buf[..n];
                        // Ctrl+] is ASCII 0x1D
                        if stdin_is_tty && slice.contains(&0x1d) {
                            on_escape();
                            break;
                        }
                        let mut written = 0;
                        while written < n {
                            let rc = unsafe {
                                libc::write(
                                    in_write_fd,
                                    slice[written..].as_ptr() as *const libc::c_void,
                                    n - written,
                                )
                            };
                            if rc <= 0 {
                                return;
                            }
                            written += rc as usize;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        }));

        Ok(Self {
            guest_read_handle,
            guest_write_handle,
            stop_flag,
            host_write_fd: Some(in_write_fd),
            threads,
            saved_termios,
        })
    }

    pub fn guest_read_handle(&self) -> &NSFileHandle {
        &self.guest_read_handle
    }

    pub fn guest_write_handle(&self) -> &NSFileHandle {
        &self.guest_write_handle
    }

    pub fn shutdown(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);
        if let Some(fd) = self.host_write_fd.take() {
            unsafe {
                libc::close(fd);
            }
        }
        if let Some(termios) = self.saved_termios.take() {
            unsafe {
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &termios);
            }
        }
        // Detach or join quickly
        for handle in self.threads.drain(..) {
            let _ = handle;
        }
    }
}

impl Drop for SerialBridge {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn create_pipe() -> Result<(RawFd, RawFd)> {
    let mut fds = [0 as RawFd; 2];
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc != 0 {
        anyhow::bail!("Failed to create serial pipe: {}", std::io::Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

fn enable_raw_stdin() -> Option<libc::termios> {
    let stdin_fd = std::io::stdin().as_raw_fd();
    if unsafe { libc::isatty(stdin_fd) } == 0 {
        return None;
    }
    let mut orig: libc::termios = unsafe { std::mem::zeroed() };
    if unsafe { libc::tcgetattr(stdin_fd, &mut orig) } != 0 {
        return None;
    }
    let mut raw = orig;
    unsafe {
        libc::cfmakeraw(&mut raw);
    }
    // Keep output post-processing so \n prints cleanly
    raw.c_oflag |= libc::OPOST;
    if unsafe { libc::tcsetattr(stdin_fd, libc::TCSANOW, &raw) } != 0 {
        return None;
    }
    Some(orig)
}
