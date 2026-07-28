//! cprog — a Linux-only progress wrapper around the system `cp`.
//!
//! Two rules govern the whole crate (docs/architecture.md):
//!   1. Decide the run policy *before* launching `cp`.
//!   2. Preserve `cp`'s result *after* it runs — cprog-side failures are warnings only.
//!
//! Module layout mirrors docs/architecture.md. Implemented test-first (docs/testing.md).
//!
//! # Public surface
//!
//! [`run`] is the only public item, and deliberately so. This crate is a CLI tool: its contract
//! is the command line — arguments handed to `cp` untouched, exit codes and signals preserved,
//! passthrough output byte-identical — not any Rust API. The modules below are implementation
//! detail and stay crate-internal, so reshaping them (which the design docs expect to keep
//! happening) is not a breaking change for anyone.

// Argument handling and run policy.
pub(crate) mod args;
pub(crate) mod plan;

// Capture, verbose line-boundary timing, slow-file detection.
pub(crate) mod capture;
pub(crate) mod verbose;
pub(crate) mod slowfile;

// Progress observation via /proc + stat.
pub(crate) mod proc;
pub(crate) mod sampler;
pub(crate) mod progress;

// Rendering.
pub(crate) mod render;
pub(crate) mod ui;

// Process lifecycle, terminal, messages, exit disposition.
pub(crate) mod process;
pub(crate) mod term;
pub(crate) mod messages;
pub(crate) mod exit;

use std::ffi::OsString;
use std::io::{self, BufWriter, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use args::ArgError;
use exit::{disposition, finalize, ExitDisposition};
use messages::{summary, version_notice, Fatal};
use plan::RunMode;
use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM, SIGTSTP, SIGWINCH};
use proc::LinuxProcSource;
use process::CommandSpec;
use progress::{ProgressState, DEFAULT_RATE_WINDOW};
use render::FooterGuard;
use sampler::{LinuxStatSource, Sampler, Tick};
use slowfile::SlowTimer;
use term::TerminalSize;
use ui::{footer_for, Footer, Style};

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
            // Best-effort: a failed stderr write (e.g. a broken pipe) must not panic and clobber
            // cp's exit disposition with 101 (docs/architecture.md "에러 철학").
            let _ = writeln!(io::stderr(), "{fatal}");
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

    let informational = inspection.as_ref().is_some_and(|insp| insp.informational);
    let (mode, verbose_present) = match &inspection {
        None => (RunMode::Passthrough, false),
        // --help/--version just print and exit; nothing to monitor, so pass through
        // (byte-identical output, no footer, no summary).
        Some(insp) if insp.informational => (RunMode::Passthrough, false),
        Some(insp) => (plan::decide(&term::detect(), insp.interactive), insp.verbose),
    };

    match mode {
        RunMode::ManagedTui => run_managed(cp_args, verbose_present),
        RunMode::Passthrough => {
            // `--help`/`--version` reach `cp` untouched, which leaves nothing naming cprog
            // itself, so one line is appended afterwards — on stderr, only on a terminal, and
            // not under CPROG_PASSTHROUGH, the user's explicit "add nothing at all" (#15,
            // docs/runtime-model.md "버전 표시"). That is the one passthrough that must
            // outlive cp; every other one execs cp in place, so the shell observes cp itself —
            // exit code, signals, job control and `$!` — with no relaying
            // (docs/process-model.md "Passthrough 생명주기").
            let notice = version_notice(
                informational && !term::passthrough_forced(),
                io::stderr().is_terminal(),
                term::detect_style(),
            );
            match notice {
                Some(text) => {
                    let outcome = run_passthrough(cp_args);
                    // Best-effort like every other write of ours.
                    let _ = writeln!(io::stderr(), "{text}");
                    outcome
                }
                None => run_passthrough_exec(cp_args),
            }
        }
    }
}

/// Run `cp` with inherited streams and wait for it, preserving its exit disposition. Only used
/// when cprog must outlive `cp` to append the version notice; every other passthrough goes
/// through [`run_passthrough_exec`].
fn run_passthrough(cp_args: &[OsString]) -> Result<ExitDisposition, Fatal> {
    let spec = CommandSpec::passthrough(cp_args);
    let mut child = process::spawn(&spec).map_err(|e| Fatal::CpSpawn(e.to_string()))?;
    let pid = child.id();
    let status = child
        .wait()
        .map_err(|e| Fatal::CpWait { pid, source: e.to_string() })?;
    Ok(disposition(status))
}

/// Replace cprog with `cp` (docs/process-model.md "Passthrough 생명주기"): on success this
/// never returns and the shell observes `cp` itself. Only an exec failure — e.g. no `cp` on
/// PATH — comes back, as the same [`Fatal::CpSpawn`] the spawn path reports.
fn run_passthrough_exec(cp_args: &[OsString]) -> Result<ExitDisposition, Fatal> {
    let spec = CommandSpec::passthrough(cp_args);
    Err(Fatal::CpSpawn(process::exec_replace(&spec).to_string()))
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

    // SIGTSTP (Ctrl-Z): flag it and handle it in the render loop so the terminal is restored
    // (cursor shown, footer erased) *before* we actually stop, and redrawn on resume. bug2/#2.
    let suspend = Arc::new(AtomicBool::new(false));
    let _ = signal_hook::flag::register(SIGTSTP, Arc::clone(&suspend));

    // Row one names the file only when nothing else does: with `-v` the line just above already
    // says it, so the row stays blank and acts as a separator instead (#20).
    let show_name = !verbose_present;
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
    // Set by the render loop when we come back from a job-control stop, so the sampler drops the
    // timing history that spans the suspend instead of averaging it in as dead time (#9).
    let resumed = Arc::new(AtomicBool::new(false));
    // Bounded on purpose: an unbounded queue removes the backpressure the pipe used to provide,
    // so a terminal slower than cp's output accumulates in memory instead of making cp wait
    // (docs/architecture.md "동시성"). Each item is one read, at most 8 KiB.
    let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(RELAY_QUEUE_DEPTH);

    // stdout carries `-v` (pulses the slow timer) and is relayed; stderr is relayed only.
    let stdout_reader = {
        let slow = Arc::clone(&slow);
        let tx = tx.clone();
        thread::spawn(move || capture::relay_stdout(stdout, &slow, &tx, verbose_present))
    };
    let stderr_reader = thread::spawn(move || capture::relay_bytes(stderr, &tx)); // moves last tx

    // Sample /proc+stat only while the current file is slow (avoids stat storms on small files).
    let sampler_thread = {
        let slow = Arc::clone(&slow);
        let progress = Arc::clone(&progress);
        let stop = Arc::clone(&stop);
        let resumed = Arc::clone(&resumed);
        thread::spawn(move || {
            let (proc_src, stat_src) = (LinuxProcSource, LinuxStatSource);
            let mut sampler = Sampler::new(&proc_src, &stat_src, pid, DEFAULT_RATE_WINDOW);
            while !stop.load(Ordering::Relaxed) {
                let now = Instant::now();
                if resumed.swap(false, Ordering::Relaxed) {
                    // cp was stopped alongside us, so the span across the suspend measured no
                    // throughput. Drop it rather than report a near-zero rate for a whole window.
                    sampler.reset_rate_history();
                }
                if lock_shared(&slow).is_slow(now) {
                    match sampler.tick(now) {
                        Tick::Sample(state) => *lock_shared(&progress) = Some(state),
                        // A read failed: keep the last value rather than make the bar flicker.
                        Tick::Skip => {}
                        // Nothing is being copied (the file finished, or we are between files),
                        // so take the bar down instead of freezing it (#7).
                        Tick::Idle => *lock_shared(&progress) = None,
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
    // Set once we resume from a suspend in the background (Ctrl-Z then `bg`): the footer is then
    // suppressed so a background job never takes over the terminal (bug1/#1 seam).
    let mut suppressed = false;
    {
        // Buffered on purpose: `StdoutLock` is line-buffered, so relaying several drained chunks
        // would flush once per chunk. Every write path here ends in an explicit `flush`, so
        // buffering changes nothing about when output appears (#18).
        let mut guard = FooterGuard::new(BufWriter::new(io::stdout().lock()));
        let mut size = TerminalSize::new(80, 24);
        // Reused across iterations: `clear` keeps the outer allocation, so draining the queue
        // costs nothing extra on the path that runs once per file (#18).
        let mut batch: Vec<Vec<u8>> = Vec::new();
        let mut last_size_query = Instant::now();
        const SIZE_FALLBACK: Duration = Duration::from_secs(1);
        // What to draw on this wake-up. Both arms of the receive below need exactly this, and a
        // suppression rule living in two places is a rule that can be half-changed.
        //
        // `suppressed` is a parameter rather than a capture because the Ctrl-Z branch assigns to
        // it while this closure is alive.
        //
        // The terminal size is only ever used to lay out a footer, so it is refreshed only when
        // one is about to be drawn — on a SIGWINCH event or the low-frequency fallback. With
        // nothing to draw this skips an ioctl *and* a clock read per wake-up, which is the whole
        // of the per-file cost on a copy of many small files (#18). Leaving the SIGWINCH flag set
        // when we skip is deliberate: the first draw that needs it consumes it.
        let mut footer_this_tick = |suppressed: bool| -> Option<Footer> {
            if suppressed || lock_shared(&progress).is_none() {
                return None;
            }
            let stale = term::should_requery_size(
                resized.swap(false, Ordering::Relaxed),
                last_size_query.elapsed(),
                SIZE_FALLBACK,
            );
            if stale {
                size = term::terminal_size(libc::STDOUT_FILENO).unwrap_or(size);
                last_size_query = Instant::now();
            }
            footer_now(&progress, size, style, show_name)
        };
        loop {
            // A caught terminating signal ends the render loop so cleanup can run.
            if received_signal.load(Ordering::Relaxed) != 0 {
                break;
            }
            // Ctrl-Z (SIGTSTP): restore the terminal, then actually stop. We stop with SIGSTOP so
            // the SIGTSTP flag handler stays installed for the next suspend; on resume (SIGCONT)
            // we re-query the size and redraw. Without this, the default stop would leave the
            // cursor hidden and a stale footer on screen (bug2/#2).
            if suspend.swap(false, Ordering::Relaxed) {
                let _ = guard.suspend_restore();
                // signal-hook's raise is a safe, async-signal-safe wrapper; it returns once we
                // are continued (SIGCONT).
                let _ = signal_hook::low_level::raise(libc::SIGSTOP);
                // On resume, only take the terminal back over if we're the foreground process
                // group again (Ctrl-Z then `fg`). If resumed in the background (Ctrl-Z then
                // `bg`), suppress the footer so we don't take over the terminal (bug1/#1 seam).
                suppressed = !term::is_foreground(libc::STDOUT_FILENO);
                resumed.store(true, Ordering::Relaxed); // drop rate history spanning the stop (#9)
                resized.store(true, Ordering::Relaxed); // requery size + redraw if not suppressed
                continue;
            }
            // Try first without a deadline: when a copy is producing `-v` lines steadily the
            // queue is never empty, and `recv_timeout` would compute a deadline — several clock
            // reads — on every single one of them. Only an empty queue needs the timed wait, and
            // that is exactly when there is time to spare (#18).
            let received = match rx.try_recv() {
                Ok(bytes) => Ok(bytes),
                Err(TryRecvError::Disconnected) => Err(RecvTimeoutError::Disconnected),
                Err(TryRecvError::Empty) => rx.recv_timeout(render_tick),
            };
            match received {
                Ok(bytes) => {
                    // Drain whatever else is already queued and write it in one pass: relaying
                    // each chunk separately would repeat the erase -> write -> draw round trip per
                    // read and let the queue outrun the terminal on a large recursive copy. The
                    // chunks stay separate — the writer buffers, so joining them would only cost
                    // a copy (#18).
                    batch.clear();
                    batch.push(bytes);
                    while let Ok(more) = rx.try_recv() {
                        batch.push(more);
                    }
                    let footer = footer_this_tick(suppressed);
                    progress_shown |= footer.is_some();
                    let rows = footer.as_ref().map(Footer::rows);
                    let _ = guard.write_log_chunks(batch.iter().map(Vec::as_slice), rows.as_ref().map(|r| &r[..]));
                }
                Err(RecvTimeoutError::Timeout) => {
                    let footer = footer_this_tick(suppressed);
                    progress_shown |= footer.is_some();
                    let _ = match footer {
                        // Withheld while an unterminated log line is on screen, so the periodic
                        // redraw cannot clobber it either (#4, docs/ui.md invariant 11).
                        Some(f) => guard.draw_unless_line_pending(&f.rows()),
                        None => guard.erase(),
                    };
                }
                // Both readers finished -> cp closed its pipes -> it is exiting.
                Err(RecvTimeoutError::Disconnected) => break,
            }
        }
        // guard's Drop erases the footer on the way out (docs/testing.md C7).
    }
    // Nothing will read the relay channel from here on. With a bounded channel that matters: a
    // reader blocked in `send` would never return and its join below would hang. Dropping the
    // receiver makes those sends fail at once so the readers finish.
    drop(rx);

    // The render loop is gone, so nothing acts on the SIGTSTP flag anymore. Hand Ctrl-Z back to
    // its default disposition (stop the group) so a suspend during teardown (join/wait) isn't
    // trapped-and-swallowed into a wedge (docs/process-model.md "정리").
    restore_default_suspend();

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
            // A *stopped* cp acts on nothing: the signal above just sits pending, its pipes stay
            // open, and the joins below would never return. Continue it so it can actually take
            // the signal and die (#5). Harmless no-op when cp is already running.
            libc::kill(pid as libc::pid_t, libc::SIGCONT);
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
    if let Some(line) = summary(&disp, start.elapsed(), style, progress_shown) {
        // Best-effort, same rationale as the Fatal path: never let a stderr write failure panic.
        let _ = writeln!(io::stderr(), "{line}");
    }
    Ok(disp)
}

/// The footer to draw right now for the given (cached) terminal size.
///
/// Only the sampler's published snapshot is consulted — it alone decides whether the current file
/// is slow. Deciding again here would cost a clock read and a second mutex on every wake-up, on
/// the path that runs once per file during a large recursive copy (#18).
fn footer_now(
    progress: &Mutex<Option<ProgressState>>,
    size: TerminalSize,
    style: Style,
    show_name: bool,
) -> Option<Footer> {
    footer_for(lock_shared(progress).as_ref(), size, style, show_name)
}

/// Lock shared render state, tolerating a poisoned mutex. The shared values — the slow-file
/// timer and the latest progress snapshot — have no invariant that a panicking holder could
/// break, so recovering the inner value keeps the main loop alive to still wait on `cp` and
/// preserve its exit code (docs/architecture.md "에러 철학").
pub(crate) fn lock_shared<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// How many captured chunks may sit unrendered before the reader threads block, restoring the
/// pipe's backpressure. Each chunk is at most one 8 KiB read, so this caps the relay backlog at
/// roughly half a megabyte (docs/architecture.md "동시성").
const RELAY_QUEUE_DEPTH: usize = 64;

/// Read a millisecond duration from an env var, falling back to `default_ms`.
fn env_ms(var: &str, default_ms: u64) -> Duration {
    ms_or_default(std::env::var(var).ok().as_deref(), default_ms)
}

/// The rule behind [`env_ms`], split out so it can be tested without touching the environment
/// (`std::env::set_var` is `unsafe` in edition 2024 and would race the other tests anyway).
///
/// Anything that is not a plain non-negative integer — a typo, a unit suffix, a negative, an
/// empty value — falls back to the default **silently** (docs/exceptions.md H1). Warning would
/// mean cprog writing a line `cp` would not have written, on the one hand, and on the other the
/// consequence of the fallback is a timing knob reverting to its documented default, which is
/// not something a user needs to act on.
fn ms_or_default(raw: Option<&str>, default_ms: u64) -> Duration {
    Duration::from_millis(raw.and_then(|s| s.parse::<u64>().ok()).unwrap_or(default_ms))
}

/// Restore `SIGTSTP` (Ctrl-Z) to its default disposition after the managed render loop ends.
///
/// During the loop the loop itself acts on a `signal_hook` flag (restore the terminal, then
/// self-`SIGSTOP`). Once the loop is gone that flag has no reader, and `signal_hook`'s handler
/// stays installed — so a Ctrl-Z during teardown would be silently swallowed (neither suspending
/// nor progressing). `signal_hook`'s `unregister` would only make it *ignore* the signal, not
/// restore the default, so reset the handler directly (docs/process-model.md "정리").
fn restore_default_suspend() {
    // SAFETY: resetting a signal to its default disposition is a valid, async-signal-safe call.
    unsafe {
        libc::signal(SIGTSTP, libc::SIG_DFL);
    }
}

#[cfg(test)]
mod env_knobs {
    use super::*;

    /// H1: anything that is not a plain non-negative integer falls back, and does so silently.
    /// The knobs are undocumented-ish timing controls, so a typo must not change behaviour in a
    /// way the user cannot see — it reverts to the documented default and says nothing.
    #[test]
    fn a_non_numeric_knob_falls_back_to_its_default() {
        for bad in ["", "abc", "100ms", "-1", "1.5", " 100", "100 ", "0x64", "٤٢"] {
            assert_eq!(
                ms_or_default(Some(bad), 125),
                Duration::from_millis(125),
                "{bad:?} is not a plain integer and must fall back"
            );
        }
        assert_eq!(ms_or_default(None, 125), Duration::from_millis(125), "unset falls back too");
    }

    #[test]
    fn a_numeric_knob_is_taken_verbatim() {
        assert_eq!(ms_or_default(Some("0"), 125), Duration::ZERO, "H2: zero is a real value");
        assert_eq!(ms_or_default(Some("1"), 125), Duration::from_millis(1));
        assert_eq!(ms_or_default(Some("60000"), 125), Duration::from_millis(60_000));
    }

    #[test]
    fn an_overflowing_knob_falls_back_rather_than_wrapping() {
        // u64::MAX + 1: parse fails, so it reverts to the default instead of wrapping into a
        // small — and silently wrong — duration.
        assert_eq!(ms_or_default(Some("18446744073709551616"), 125), Duration::from_millis(125));
    }
}

#[cfg(test)]
mod teardown_signal_disposition {
    use super::*;

    /// After the render loop, SIGTSTP must revert to its default disposition. Installing the same
    /// `signal_hook` flag `run_managed` uses and then tearing down must leave the OS handler at
    /// `SIG_DFL` — otherwise a Ctrl-Z during teardown would be trapped and swallowed (a wedge).
    #[test]
    fn suspend_reverts_to_default_after_render_loop() {
        let flag = Arc::new(AtomicBool::new(false));
        let id = signal_hook::flag::register(SIGTSTP, Arc::clone(&flag)).expect("register SIGTSTP");

        restore_default_suspend();

        // Query (act = NULL) the current SIGTSTP disposition; it must be the default handler.
        let mut cur: libc::sigaction = unsafe { std::mem::zeroed() };
        // SAFETY: reading the current SIGTSTP action into a zeroed, owned sigaction struct.
        unsafe { libc::sigaction(SIGTSTP, std::ptr::null(), &mut cur) };
        assert_eq!(
            cur.sa_sigaction,
            libc::SIG_DFL,
            "SIGTSTP must be restored to SIG_DFL after the render loop, not left trapped"
        );

        let _ = signal_hook::low_level::unregister(id);
    }
}
