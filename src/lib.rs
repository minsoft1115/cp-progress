//! cprog — a Linux-only progress wrapper around the system `cp`.
//!
//! Two rules govern the whole crate (docs/architecture.md):
//!   1. Decide the run policy *before* launching `cp`.
//!   2. Preserve `cp`'s result *after* it runs — cprog-side failures are warnings only.
//!
//! Module layout mirrors docs/architecture.md. Implemented test-first (docs/testing.md).

// Argument handling and run policy.
pub mod args;
pub mod plan;

// Capture, verbose line-boundary timing, slow-file detection.
pub mod capture;
pub mod verbose;
pub mod slowfile;

// Progress observation via /proc + stat.
pub mod proc;
pub mod sampler;
pub mod progress;

// Rendering.
pub mod render;
pub mod ui;

// Process lifecycle, terminal, messages, exit disposition.
pub mod process;
pub mod term;
pub mod messages;
pub mod exit;

use std::ffi::OsString;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use args::ArgError;
use exit::{disposition, finalize, ExitDisposition};
use messages::{summary, Fatal};
use plan::RunMode;
use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGWINCH};
use proc::LinuxProcSource;
use process::CommandSpec;
use progress::{ProgressState, DEFAULT_RATE_WINDOW};
use render::FooterGuard;
use sampler::{LinuxStatSource, Sampler};
use slowfile::SlowTimer;
use term::TerminalSize;
use ui::{footer_for, Style};

/// Top-level orchestration entry point (docs/architecture.md "상위 흐름").
///
/// Collects the `cp` arguments, decides the run mode, and runs `cp`, returning the exit code
/// cprog should exit with. Fatal problems are printed to stderr and mapped to their code.
pub fn run() -> i32 {
    let cp_args: Vec<OsString> = std::env::args_os().skip(1).collect();
    match dispatch(&cp_args) {
        // Preserve cp's exit disposition: a signal is re-raised, a code returned.
        Ok(disp) => finalize(disp),
        Err(fatal) => {
            eprintln!("{fatal}");
            fatal.code()
        }
    }
}

/// Decide the mode and run `cp`, returning cp's exit disposition or a [`Fatal`] to report.
fn dispatch(cp_args: &[OsString]) -> Result<ExitDisposition, Fatal> {
    let inspection = match args::inspect(cp_args) {
        Ok(insp) => Some(insp),
        Err(ArgError::Empty) => return Err(Fatal::Usage),
        // A failed scan can't be trusted; fall back to passthrough conservatively.
        Err(ArgError::Scan(_)) => None,
    };

    let (mode, verbose_present) = match &inspection {
        None => (RunMode::Passthrough, false),
        // --help/--version just print and exit; nothing to monitor, so pass through
        // (byte-identical output, no footer, no summary).
        Some(insp) if insp.informational => (RunMode::Passthrough, false),
        Some(insp) => (plan::decide(&term::detect(), insp.interactive), insp.verbose),
    };

    match mode {
        RunMode::Passthrough => run_passthrough(cp_args),
        RunMode::ManagedTui => run_managed(cp_args, verbose_present),
    }
}

/// Run `cp` with inherited streams, byte-identical to `cp`, and preserve its exit disposition.
fn run_passthrough(cp_args: &[OsString]) -> Result<ExitDisposition, Fatal> {
    let spec = CommandSpec::passthrough(cp_args);
    let mut child = process::spawn(&spec).map_err(|e| Fatal::CpSpawn(e.to_string()))?;
    let pid = child.id();
    let status = child
        .wait()
        .map_err(|e| Fatal::CpWait { pid, source: e.to_string() })?;
    Ok(disposition(status))
}

/// Run `cp` under the managed TUI (docs/process-model.md "Managed 생명주기").
///
/// `cp` is launched with captured stdout/stderr; reader threads relay those bytes through the
/// sole-writer footer and pulse the slow-file timer on each `-v` line; a sampler thread polls
/// `/proc`+`stat` while the current file is slow; and the main thread renders the footer and
/// relays the log region. On exit the footer is cleared and a summary is printed to stderr.
fn run_managed(cp_args: &[OsString], verbose_present: bool) -> Result<ExitDisposition, Fatal> {
    let spec = CommandSpec::managed(cp_args, verbose_present);
    let mut child = process::spawn(&spec).map_err(|e| Fatal::CpSpawn(e.to_string()))?;
    let pid = child.id();
    let stdout = child.stdout.take().expect("managed mode captures stdout");
    let stderr = child.stderr.take().expect("managed mode captures stderr");

    // Catch terminating signals so cprog isn't killed mid-render: the loop breaks, the footer is
    // cleared, and cp's true signaled status is re-raised on exit (docs/process-model.md). We
    // record *which* signal arrived (0 = none) so a signal delivered to cprog alone can be
    // forwarded to cp as itself, rather than normalized to SIGTERM.
    let received_signal = Arc::new(AtomicI32::new(0));
    for sig in [SIGINT, SIGTERM, SIGHUP, SIGQUIT] {
        let received = Arc::clone(&received_signal);
        // SAFETY: the handler only stores the signal number into an atomic — async-signal-safe.
        let _ = unsafe {
            signal_hook::low_level::register(sig, move || received.store(sig, Ordering::Relaxed))
        };
    }

    // SIGWINCH marks the size dirty; the render loop re-queries on this event or a
    // low-frequency fallback (covers missed/coalesced signals, docs/runtime-model.md).
    let resized = Arc::new(AtomicBool::new(true)); // true -> query size on first render
    let _ = signal_hook::flag::register(SIGWINCH, Arc::clone(&resized));

    let style = term::detect_style();
    let threshold = env_ms(
        "CPROG_SLOW_THRESHOLD_MS",
        slowfile::DEFAULT_SLOW_THRESHOLD.as_millis() as u64,
    );
    let sample_interval = env_ms("CPROG_SAMPLE_INTERVAL_MS", 100);
    let render_tick = env_ms("CPROG_RENDER_TICK_MS", 125);

    let slow = Arc::new(Mutex::new(SlowTimer::new(threshold)));
    let progress: Arc<Mutex<Option<ProgressState>>> = Arc::new(Mutex::new(None));
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = mpsc::channel::<Vec<u8>>();

    // stdout carries `-v` (pulses the slow timer) and is relayed; stderr is relayed only.
    let stdout_reader = {
        let slow = Arc::clone(&slow);
        let tx = tx.clone();
        thread::spawn(move || capture::relay_stdout(stdout, &slow, &tx))
    };
    let stderr_reader = thread::spawn(move || capture::relay_bytes(stderr, &tx)); // moves last tx

    // Sample /proc+stat only while the current file is slow (avoids stat storms on small files).
    let sampler_thread = {
        let slow = Arc::clone(&slow);
        let progress = Arc::clone(&progress);
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            let (proc_src, stat_src) = (LinuxProcSource, LinuxStatSource);
            let mut sampler = Sampler::new(&proc_src, &stat_src, pid, DEFAULT_RATE_WINDOW);
            while !stop.load(Ordering::Relaxed) {
                let now = Instant::now();
                if lock_shared(&slow).is_slow(now) {
                    if let Some(state) = sampler.tick(now) {
                        *lock_shared(&progress) = Some(state);
                    }
                } else {
                    *lock_shared(&progress) = None;
                }
                thread::sleep(sample_interval);
            }
        })
    };

    // Main thread is the sole writer: relay log bytes and redraw the footer on each tick.
    let start = Instant::now();
    // Whether the footer ever engaged (a slow file was actually monitored). Gates the summary:
    // if cp did nothing worth showing (e.g. --help, an instant exit), we stay quiet.
    let mut progress_shown = false;
    {
        let mut guard = FooterGuard::new(io::stdout().lock());
        let mut size = TerminalSize::new(80, 24);
        let mut last_size_query = Instant::now();
        const SIZE_FALLBACK: Duration = Duration::from_secs(1);
        loop {
            // A caught terminating signal ends the render loop so cleanup can run.
            if received_signal.load(Ordering::Relaxed) != 0 {
                break;
            }
            // Refresh the terminal size on a SIGWINCH event or the low-frequency fallback,
            // rather than an ioctl every tick.
            if term::should_requery_size(resized.swap(false, Ordering::Relaxed), last_size_query.elapsed(), SIZE_FALLBACK) {
                size = term::terminal_size(libc::STDOUT_FILENO).unwrap_or(size);
                last_size_query = Instant::now();
            }
            match rx.recv_timeout(render_tick) {
                Ok(bytes) => {
                    let footer = footer_now(&slow, &progress, size, style);
                    progress_shown |= footer.is_some();
                    let _ = guard.write_log(&bytes, footer.as_deref());
                }
                Err(RecvTimeoutError::Timeout) => {
                    let footer = footer_now(&slow, &progress, size, style);
                    progress_shown |= footer.is_some();
                    let _ = match footer {
                        Some(text) => guard.draw(&text),
                        None => guard.erase(),
                    };
                }
                // Both readers finished -> cp closed its pipes -> it is exiting.
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        // guard's Drop erases the footer on the way out (docs/testing.md C7).
    }

    // If we were signaled, cp may still be running — a signal delivered to cprog alone (e.g.
    // `kill <cprog>`) rather than the whole foreground group. In that case the reader threads are
    // blocked in read() on cp's still-open pipes, so joining them would hang. Forward the *same*
    // signal to cp unless it has already exited: cp then dies of that signal, the pipes close,
    // the joins stay bounded, and cprog re-raises the matching signal below. A `try_wait` error
    // (e.g. EINTR racing signal delivery) is treated as "maybe still alive" and falls through to
    // the kill defensively — a stray signal to an exited pid is harmless.
    let received = received_signal.load(Ordering::Relaxed);
    if received != 0 && !matches!(child.try_wait(), Ok(Some(_))) {
        // SAFETY: valid pid and signal; a race where cp already exited yields ESRCH, ignored.
        unsafe {
            libc::kill(pid as libc::pid_t, received);
        }
    }
    stop.store(true, Ordering::Relaxed);
    let _ = stdout_reader.join();
    let _ = stderr_reader.join();
    let _ = sampler_thread.join();

    let status = child
        .wait()
        .map_err(|e| Fatal::CpWait { pid, source: e.to_string() })?;
    let disp = disposition(status);
    if let Some(line) = summary(&disp, start.elapsed(), style.color, progress_shown) {
        eprintln!("{line}");
    }
    Ok(disp)
}

/// The footer to draw right now for the given (cached) terminal size: slow-file state +
/// latest sample.
fn footer_now(
    slow: &Mutex<SlowTimer>,
    progress: &Mutex<Option<ProgressState>>,
    size: TerminalSize,
    style: Style,
) -> Option<String> {
    let now = Instant::now();
    let is_slow = lock_shared(slow).is_slow(now);
    let state = lock_shared(progress);
    footer_for(is_slow, state.as_ref(), size, style)
}

/// Lock shared render state, tolerating a poisoned mutex. The shared values — the slow-file
/// timer and the latest progress snapshot — have no invariant that a panicking holder could
/// break, so recovering the inner value keeps the main loop alive to still wait on `cp` and
/// preserve its exit code (docs/architecture.md "에러 철학").
pub(crate) fn lock_shared<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Read a millisecond duration from an env var, falling back to `default_ms`.
fn env_ms(var: &str, default_ms: u64) -> Duration {
    let ms = std::env::var(var).ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(default_ms);
    Duration::from_millis(ms)
}
