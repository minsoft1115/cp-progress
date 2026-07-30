//! Shared helpers for the PTY-based integration tests.
//!
//! Not every test crate uses every helper, so unused-warnings are allowed here.
#![allow(dead_code)]

use nix::fcntl::{FcntlArg, FdFlag, OFlag};
use nix::pty::{OpenptyResult, Winsize};
use nix::sys::stat::Mode;
use nix::sys::termios::Termios;
use std::ffi::CString;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a FIFO write-end open waits for `cp` to show up as the reader.
///
/// Generous enough that a loaded machine spawning `cp` is never mistaken for a broken one, and
/// short enough that the test still reports before any CI job timeout.
const FIFO_READER_TIMEOUT: Duration = Duration::from_secs(10);

/// `openpty` with `FD_CLOEXEC` on both ends.
///
/// **The flag is what keeps one test from failing another one.** These tests run in parallel in
/// one process, and each spawns children. A PTY fd without `FD_CLOEXEC` survives the `exec` of
/// *every sibling test's* child, so an unrelated `cp` ends up holding this test's slave open —
/// and a master whose slave is still open never reports EOF. The test that owns the PTY then
/// blocks until its own deadline and is reported as the failure, while the test that actually
/// leaked nothing and broke nothing is the one that inherited the fd. The misattribution is not
/// hypothetical: raising `SIZE_FALLBACK` to an hour made `sigwinch_relayouts_the_footer_to_the_new_width`
/// pass alone in 0.10 s and fail in 10.05 s alongside its own suite — *both* resize tests red,
/// each held open by the other's child (#69).
///
/// The `Stdio` handles the tests hand their children are `try_clone`s, and `dup2` onto fd 0/1/2
/// clears `FD_CLOEXEC` on the copy — so deliberate stdio inheritance still works. Only the
/// undeliberate kind stops.
pub fn openpty_cloexec(winsize: Option<&Winsize>) -> OpenptyResult {
    let pty = nix::pty::openpty(winsize, None::<&Termios>).expect("openpty");
    for fd in [pty.master.as_fd(), pty.slave.as_fd()] {
        nix::fcntl::fcntl(fd, FcntlArg::F_SETFD(FdFlag::FD_CLOEXEC)).expect("set FD_CLOEXEC on pty fd");
    }
    pty
}

/// Open the write end of a FIFO, giving up if no reader ever arrives.
///
/// A blocking `open(2)` of a FIFO's write end does not return until some process opens the read
/// end. In this suite that reader is always the `cp` that the cprog under test spawns — so a
/// regression that stops cprog from reaching the spawn at all (a broken managed launch, a wrong
/// mode decision) leaves this open blocking **forever**, and the test never reports.
///
/// [`Watchdog`] cannot rescue that. It kills the child, and the child was never what was holding
/// the open — the kernel is, waiting for a reader that is not coming. Pointing `stdbuf` at a
/// non-existent program showed the shape: 16 tests across five suites stopped reporting entirely
/// and the run ended as a bare CI timeout with no test named (#69).
///
/// `O_NONBLOCK` turns "wait for a reader" into `ENXIO`, which this polls on until the deadline and
/// then reports as `None` — a red assertion at the call site instead of silence. On success the
/// flag is cleared again, so the caller's `write_all` throttles against a full pipe buffer exactly
/// as it would have after a blocking open.
pub fn open_fifo_writer(fifo: &std::path::Path) -> Option<File> {
    let deadline = Instant::now() + FIFO_READER_TIMEOUT;
    loop {
        match nix::fcntl::open(fifo, OFlag::O_WRONLY | OFlag::O_NONBLOCK, Mode::empty()) {
            Ok(fd) => {
                // F_SETFL ignores the access-mode bits, so an empty set clears O_NONBLOCK without
                // disturbing O_WRONLY. Blocking writes are what the throttled feeders assume: a
                // non-blocking one would return EAGAIN on a full pipe and be read as "cp closed".
                nix::fcntl::fcntl(fd.as_fd(), FcntlArg::F_SETFL(OFlag::empty())).ok()?;
                return Some(File::from(fd));
            }
            // Precisely "no reader has opened this FIFO yet" for O_WRONLY | O_NONBLOCK.
            Err(nix::errno::Errno::ENXIO) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(_) => return None,
        }
    }
}

/// Read one chunk from a PTY master, retrying on EINTR and treating EIO (the slave side fully
/// closed) or any other error as end-of-stream. Returns 0 at EOF. This keeps the integration
/// tests from mistaking a signal-interrupted read for the end of cprog's output.
///
/// **Folding every error into EOF means a caller can get an empty buffer and not know why**, so a
/// test that only asserts an absence ("no bar", "no `ESC[K`") would pass on a read that never
/// happened. Every such test in this suite therefore carries a positive guard first — the
/// destination's length, `copied > 0`, a substring that must be present — and that guard is not
/// optional (#60).
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
///
/// Deliberately not the same function as `ui.rs`'s unit-test helper of the same name — the crate
/// boundary puts that one out of reach, and this one has a harder input. It reads raw PTY output,
/// so it drops control characters too (`\r`, `\n`, cursor movement); the unit-test copy takes a
/// single composed footer row where a control character would itself be the bug.
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
///
/// Like [`read_retry`], a read error ends the drain silently rather than failing: the caller sees
/// a short buffer, not an error. Callers must assert something *positive* before asserting an
/// absence (#60).
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

/// Print a notice that survives libtest's output capture.
///
/// `eprintln!` is captured and thrown away for a passing test, so a skip announced that way is
/// invisible — exactly the silence it was meant to break. A direct write to the real stderr is
/// not intercepted (#61 D).
pub fn notice(msg: &str) {
    use std::io::Write;
    let _ = std::io::stderr().write_all(format!("{msg}\n").as_bytes());
}

/// Kills `pid` if the test has not disarmed within `secs`, and remembers that it had to.
///
/// cprog wraps signals, job control and PTYs, so a regression here does not usually produce a
/// wrong byte — it produces **no bytes at all**, and a test whose only exit is EOF on the master
/// then blocks forever. A hang is the one failure a suite cannot report: CI shows a timeout with
/// no test named. This turns that into a normal red assertion (#61 D).
///
/// **Every test whose only exit is EOF arms one.** That was not true until #69: turning the render
/// loop's `Disconnected => break` into `continue` left 12 tests running forever (quiet 4,
/// log_integrity 1, managed 6, fallback 1), and sleeping before `cmd.exec()` in the passthrough
/// path left 3 more (forced_passthrough 2, background 1) — the last of which had a 30 s poll
/// deadline that an unconditional `child.wait()` after it made moot. A bounded read loop is not
/// enough on its own; the wait that follows it has to be bounded too.
///
/// **Disarm immediately after `wait()`.** The kill lands on a pid the test has not reaped yet in
/// every ordinary path, so it can only hit its own child — the one exception is the window
/// between the test reaping the child and calling `disarm()`, where the pid could in principle be
/// reused. That window is microseconds and closing it entirely would need the watchdog to
/// re-identify the process; keeping `disarm()` on the line after `wait()` is the cheaper rule.
/// `Drop` disarms as a backstop and joins.
pub struct Watchdog {
    hung: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Watchdog {
    pub fn arm(pid: i32, secs: u64) -> Self {
        let hung = Arc::new(AtomicBool::new(false));
        let done = Arc::new(AtomicBool::new(false));
        let (h, d) = (Arc::clone(&hung), Arc::clone(&done));
        let handle = std::thread::spawn(move || {
            for _ in 0..(secs * 10) {
                std::thread::sleep(Duration::from_millis(100));
                if d.load(Ordering::Relaxed) {
                    return;
                }
            }
            h.store(true, Ordering::Relaxed);
            unsafe { libc::kill(pid, libc::SIGKILL) };
        });
        Watchdog { hung, done, handle: Some(handle) }
    }

    /// Whether the watchdog had to kill the child — i.e. the test would have hung.
    pub fn hung(&self) -> bool {
        self.hung.load(Ordering::Relaxed)
    }

    pub fn disarm(&self) {
        self.done.store(true, Ordering::Relaxed);
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.disarm();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// A throttled writer feeding a FIFO to keep a `cp` copy slow. Stops and joins on drop.
///
/// A FIFO that cannot be opened turns the thread into a no-op, which turns "a slow copy" into an
/// instant one with nothing said about it. That is deliberate — a failure here is not the test's
/// subject — but it is another reason the tests that use it must first prove the copy was slow
/// (a footer appeared, `copied > 0`) before concluding anything from what is missing (#60).
///
/// The open goes through [`open_fifo_writer`], which bounds the wait for a reader. Without that
/// bound "no-op" was not the failure mode at all: the thread blocked in `open` forever, and
/// `Drop`'s join then blocked the test that owned it — so a missing reader took the *test process*
/// down with it rather than merely weakening one assertion (#69).
pub struct Feeder {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}
impl Feeder {
    pub fn start(fifo: std::path::PathBuf) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            let Some(mut w) = open_fifo_writer(&fifo) else { return };
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
/// Four sequences are interpreted, and they are exactly the ones `FooterGuard` emits: `\r`
/// (column 0), `\n` (next row), `CSI K` (erase to end of line) and **`CSI A` (up one row)**.
/// Anything else escape-like is skipped, and ordinary bytes overwrite whatever is under the
/// cursor. That is what makes it possible to assert on *visible* output rather than on bytes that
/// were written and then overdrawn.
///
/// `CSI A` is not optional decoration: the two-row footer's erase is `\r CSI K CSI A \r CSI K`,
/// so ignoring the cursor movement clears the second row twice and leaves the first one standing
/// — a row the terminal had wiped stays on the modelled screen, and every negative assertion
/// ("no line starts with `:`") is then answered by a screen that never existed (#60).
///
/// Rows are kept in a vector rather than pushed on `\n`, because the cursor has to be able to go
/// back to one. `src/render.rs`'s `Screen` does the same thing for the unit tests; the difference
/// is that this replay has no bottom edge and therefore does not scroll — it is reconstructing a
/// capture, not simulating a terminal of a fixed height.
pub fn render_screen(bytes: &[u8]) -> Vec<String> {
    let mut rows: Vec<Vec<u8>> = vec![Vec::new()];
    let mut row = 0usize;
    let mut col = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'\r' => {
                col = 0;
                i += 1;
            }
            b'\n' => {
                row += 1;
                if row == rows.len() {
                    rows.push(Vec::new());
                }
                // The captures come from `openpty(.., None::<&Termios>)` — a default line
                // discipline, where ONLCR turns a newline into CR+LF — so returning to column 0
                // is what the terminal did. `FooterGuard` writes its own `\r` before every row
                // regardless, so cprog's output renders the same either way.
                col = 0;
                i += 1;
            }
            0x1b => {
                if bytes[i..].starts_with(b"\x1b[K") {
                    rows[row].truncate(col); // erase to end of line
                    i += 3;
                } else if bytes[i..].starts_with(b"\x1b[A") {
                    row = row.saturating_sub(1); // up one row, column unchanged
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
                let line = &mut rows[row];
                if col < line.len() {
                    line[col] = b;
                } else {
                    line.resize(col, b' ');
                    line.push(b);
                }
                col += 1;
                i += 1;
            }
        }
    }
    rows.iter().map(|l| String::from_utf8_lossy(l).into_owned()).collect()
}
