//! Shared helpers for the PTY-based integration tests.
//!
//! Not every test crate uses every helper, so unused-warnings are allowed here.
#![allow(dead_code)]

use std::ffi::CString;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Read one chunk from a PTY master, retrying on EINTR and treating EIO (the slave side fully
/// closed) or any other error as end-of-stream. Returns 0 at EOF. This keeps the integration
/// tests from mistaking a signal-interrupted read for the end of cprog's output.
pub fn read_retry(master: &mut File, buf: &mut [u8]) -> usize {
    loop {
        return match master.read(buf) {
            Ok(n) => n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(_) => 0,
        };
    }
}

/// A throwaway temp directory, removed on drop.
pub struct TmpDir(pub std::path::PathBuf);
impl TmpDir {
    pub fn new(tag: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("cprog_it_{}_{}", std::process::id(), tag));
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
    /// A path to `name` inside this temp directory.
    pub fn path(&self, name: &str) -> std::path::PathBuf {
        self.0.join(name)
    }
}
impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Whether `needle` occurs anywhere in `hay`.
pub fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Index of the last occurrence of `needle` in `hay`.
pub fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    (0..=hay.len().saturating_sub(needle.len())).rev().find(|&i| &hay[i..i + needle.len()] == needle)
}

/// Strip ANSI CSI sequences so a footer line's visible width can be measured.
pub fn strip_sgr(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let mut out = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            for n in chars.by_ref() {
                if n.is_ascii_alphabetic() {
                    break;
                }
            }
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

/// Poll a fd for readability, up to `ms` milliseconds.
pub fn readable(fd: RawFd, ms: i32) -> bool {
    let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
    unsafe { libc::poll(&mut pfd, 1, ms) };
    pfd.revents & libc::POLLIN != 0
}

/// Non-blocking drain of a PTY master into `out` until `deadline`, or until `marker` (searched
/// from the given index) appears — whichever comes first.
pub fn drain(master: &mut File, fd: RawFd, out: &mut Vec<u8>, deadline: Instant, marker: Option<(usize, &[u8])>) {
    let mut buf = [0u8; 8192];
    while Instant::now() < deadline {
        if let Some((from, m)) = marker {
            if from <= out.len() && contains(&out[from..], m) {
                return;
            }
        }
        if readable(fd, 50) {
            match master.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => out.extend_from_slice(&buf[..n]),
                Err(_) => return,
            }
        }
    }
}

/// A throttled writer feeding a FIFO to keep a `cp` copy slow. Stops and joins on drop.
pub struct Feeder {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}
impl Feeder {
    pub fn start(fifo: std::path::PathBuf) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let Ok(mut w) = std::fs::OpenOptions::new().write(true).open(&fifo) else { return };
            let chunk = vec![0u8; 64 * 1024];
            while !s.load(Ordering::Relaxed) {
                if w.write_all(&chunk).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        Feeder { stop, handle: Some(handle) }
    }
}
impl Drop for Feeder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// The program path + argv + envp (as owned `CString`s) to exec `cprog` copying `fifo` -> `dst`
/// with brisk managed-mode timings and no `CI`. The caller builds the NULL-terminated pointer
/// arrays locally so the pointers stay valid across `fork`.
pub fn cprog_exec(fifo: &std::path::Path, dst: &std::path::Path) -> (CString, Vec<CString>, Vec<CString>) {
    let prog = CString::new(env!("CARGO_BIN_EXE_cprog")).unwrap();
    let argv = vec![
        CString::new("cprog").unwrap(),
        CString::new(fifo.to_str().unwrap()).unwrap(),
        CString::new(dst.to_str().unwrap()).unwrap(),
    ];
    let mut env = vec![
        "TERM=xterm".to_string(),
        "CPROG_SLOW_THRESHOLD_MS=1".to_string(),
        "CPROG_SAMPLE_INTERVAL_MS=10".to_string(),
        "CPROG_RENDER_TICK_MS=10".to_string(),
    ];
    if let Ok(p) = std::env::var("PATH") {
        env.push(format!("PATH={p}"));
    }
    let envp = env.into_iter().map(|s| CString::new(s).unwrap()).collect();
    (prog, argv, envp)
}

/// Replay a captured terminal byte stream into the lines a user would actually see.
///
/// Only the sequences cprog emits for the footer matter here: `\r` (column 0), `\n` (next line)
/// and `CSI K` (erase to end of line). Everything else that is an escape sequence is skipped and
/// ordinary bytes overwrite whatever is under the cursor. This is what makes it possible to
/// assert on *visible* output rather than on bytes that were written and then overdrawn.
pub fn render_screen(bytes: &[u8]) -> Vec<String> {
    let mut lines = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut col = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                col = 0;
                i += 1;
            }
            b'\n' => {
                lines.push(String::from_utf8_lossy(&cur).into_owned());
                cur.clear();
                col = 0;
                i += 1;
            }
            0x1b => {
                if bytes[i..].starts_with(b"\x1b[K") {
                    cur.truncate(col); // erase to end of line
                    i += 3;
                } else {
                    // Skip any other CSI/escape sequence up to its final byte.
                    i += 1;
                    if bytes.get(i) == Some(&b'[') {
                        i += 1;
                    }
                    while i < bytes.len() && !bytes[i].is_ascii_alphabetic() {
                        i += 1;
                    }
                    i += 1;
                }
            }
            b => {
                if col < cur.len() {
                    cur[col] = b;
                } else {
                    cur.resize(col, b' ');
                    cur.push(b);
                }
                col += 1;
                i += 1;
            }
        }
    }
    lines.push(String::from_utf8_lossy(&cur).into_owned());
    lines
}
