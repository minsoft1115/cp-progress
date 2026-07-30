#![cfg(feature = "integration")]
//! Terminal-resize (SIGWINCH) integration test (docs/testing.md C1).
//!
//! While cprog is drawing a footer in managed mode, shrink the PTY and deliver SIGWINCH. cprog
//! must re-query the size and redraw the footer to the new (narrower) width. (A real terminal
//! delivers SIGWINCH via the controlling terminal; the test harness sends it directly, which
//! exercises the same handling — flag -> re-query -> relayout.)
//!
//! The copy source is a FIFO fed by a throttled writer thread, so the copy stays alive for as
//! long as we keep feeding: we deterministically observe a wide footer, resize, observe a narrower
//! redraw, then stop feeding so cp hits EOF and exits. This avoids any race with copy speed (a
//! plain large regular file can finish before the post-resize redraw on fast storage).
//!
//! `sigwinch_relayouts_the_footer_to_the_new_width` runs that cycle **twice**, and the first pass
//! is not a warm-up: a narrow footer appearing at all is also what the 1 s fallback re-query
//! produces, so the signal's contribution is timing rather than outcome. The first resize is
//! deliberately unsignalled to put the fallback's clock at a known zero, and only then is the
//! second one signalled and timed (#69). The parenthetical above is why that works — the harness
//! is the sole source of SIGWINCH here.

use std::fs::File;
use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nix::pty::Winsize;
use nix::sys::signal::{kill, killpg, Signal};
use nix::unistd::Pid;

mod common;
use common::{open_fifo_writer, openpty_cloexec, read_retry, strip_sgr, TmpDir};

/// Visible widths of the footer redraws (each `\r`-delimited segment containing a `%`) in `data`.
fn footer_widths(data: &[u8]) -> Vec<usize> {
    use unicode_width::UnicodeWidthStr;
    data.split(|&b| b == b'\r')
        .map(strip_sgr)
        .filter(|t| t.contains('%'))
        .map(|t| UnicodeWidthStr::width(t.trim_end()))
        .collect()
}

#[test]
fn sigwinch_relayouts_the_footer_to_the_new_width() {
    let tmp = TmpDir::new("winch");
    let fifo = tmp.0.join("src.fifo");
    let dst = tmp.0.join("dst.bin");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();

    let ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty_cloexec(Some(&ws));
    let out_fd: OwnedFd = pty.slave.try_clone().unwrap();
    let err_fd: OwnedFd = pty.slave.try_clone().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .arg(&fifo)
        .arg(&dst)
        .env("TERM", "xterm")
        .env("LC_ALL", "C.UTF-8")
        .env_remove("CI")
        .env("CPROG_SLOW_THRESHOLD_MS", "1")
        .env("CPROG_SAMPLE_INTERVAL_MS", "8")
        .env("CPROG_RENDER_TICK_MS", "8")
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_fd))
        .stderr(Stdio::from(err_fd))
        .spawn()
        .unwrap();
    let master_fd = pty.master.as_raw_fd();
    let pgid = Pid::from_raw(child.id() as i32);
    let cprog_pid = Pid::from_raw(child.id() as i32);
    drop(pty.slave);

    // Throttled feeder: keeps the copy alive (dest growing) until told to stop.
    let stop_feed = Arc::new(AtomicBool::new(false));
    let feeder = {
        let (fifo, stop) = (fifo.clone(), Arc::clone(&stop_feed));
        std::thread::spawn(move || {
            // Waits, with a deadline, until cp opens the read end: an open that could block
            // forever would take this thread's join — and so the test — down with it (#69).
            // Rust ignores SIGPIPE, so a closed reader surfaces as a write error, not a kill.
            let Some(mut w) = open_fifo_writer(&fifo) else { return };
            let chunk = vec![0u8; 64 * 1024];
            while !stop.load(Ordering::Relaxed) {
                if w.write_all(&chunk).is_err() {
                    return; // cp closed the pipe (finished/killed)
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };

    // Watchdog: never let a stuck relayout hang the suite.
    let hung = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let (hung, done) = (Arc::clone(&hung), Arc::clone(&done));
        std::thread::spawn(move || {
            for _ in 0..100 {
                std::thread::sleep(Duration::from_millis(100));
                if done.load(Ordering::Relaxed) {
                    return;
                }
            }
            hung.store(true, Ordering::Relaxed);
            let _ = kill(cprog_pid, Signal::SIGKILL);
        })
    };

    // Three stages, because "a narrow footer eventually appeared" does not need SIGWINCH at all:
    // the 1 s fallback re-query produces one too, and deleting the `flag::register(SIGWINCH, …)`
    // line left this test — and the whole suite — green (#69). What the signal actually buys is
    // *promptness*, so promptness is what gets measured.
    //
    // Timing the signal alone would only catch the mutant by luck. The fallback fires 1 s after
    // the last query, and the phase of that cycle relative to the resize is arbitrary, so a
    // deleted registration would still relayout in anywhere from 0 to 1000 ms. Stage 2 removes the
    // luck: it resizes *without* signalling and waits for the relayout, which can only have come
    // from a fallback re-query — so when it is observed, the 1 s clock has just been reset. Stage 3
    // then resizes and signals immediately, leaving the fallback ~1 s away and the signal ~1 tick.
    // Measured: 6.6 ms signalled against 1.005 s from the fallback, so the 400 ms threshold below
    // has two orders of magnitude of room on both sides.
    //
    // Stage 2's "without signalling" is load-bearing and worth stating, because `TIOCSWINSZ` on a
    // master *can* make the kernel deliver SIGWINCH to the terminal's foreground process group. It
    // does not here: cprog never calls `setsid`/`TIOCSCTTY`, so this pty is not its controlling
    // terminal and the explicit `killpg` below is the only SIGWINCH it ever sees. Removing that
    // one call makes stage 3 take 1.004 s — the same fallback timing as deleting the registration,
    // which is what proves stage 2 relayouts on the fallback and not on a signal (#69).
    //
    // Each stage waits for the width to *drop*, rather than for an absolute number. A footer is
    // laid out against the terminal width and then trimmed of trailing blanks, and the relationship
    // is not proportional: 80 columns yields 61-63, 50 yields 42, 30 yields 30. Pinning those
    // literals would encode three incidental numbers, so the stages compare against the width they
    // measured — hence `wide` and `mid` being learned rather than written down.
    //
    // `DROP` separates a real relayout from the jitter of a changing ETA field, which is the 2
    // columns of that 61-63 spread. The two real drops are 63->42 and 42->30, so 6 leaves 3x
    // headroom over the jitter and still sits 2x under the smaller of the two drops.
    const DROP: usize = 6;
    let track: [u8; 3] = [0xE2, 0x96, 0x91]; // ░ (indeterminate bar, FIFO source has no total)
    let set_cols = |cols: u16| {
        let ws = Winsize { ws_row: 24, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 };
        unsafe { libc::ioctl(master_fd, libc::TIOCSWINSZ, &ws) };
    };
    // A region spans the redraws either side of a relayout, so "did the width drop" has to ask
    // whether *any* redraw in it is narrower — a max would keep reporting the pre-resize width
    // forever — while learning the new width is the *narrowest* one in that same region.
    let widest = |data: &[u8]| footer_widths(data).into_iter().max();
    let narrowest = |data: &[u8]| footer_widths(data).into_iter().min();
    let dropped_below = |data: &[u8], from: usize| {
        footer_widths(data).iter().any(|&w| w + DROP <= from)
    };
    let mut master = File::from(pty.master);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    // The wide footer's own width, learned rather than assumed, and stage 2's offset.
    let mut wide: Option<usize> = None;
    let mut mid_off: Option<usize> = None;
    // The 50-column width, stage 3's offset, and the instant the SIGWINCH went out.
    let mut mid: Option<usize> = None;
    let mut resize_off: Option<usize> = None;
    let mut signalled_at: Option<Instant> = None;
    let mut narrow_after: Option<Duration> = None;
    loop {
        match read_retry(&mut master, &mut buf) {
            0 => break,
            n => {
                out.extend_from_slice(&buf[..n]);
                match (mid_off, resize_off) {
                    // Stage 1 -> 2: the 80-column footer is up. Shrink to 50 and say nothing.
                    (None, _) if out.windows(3).any(|w| w == track) => {
                        wide = widest(&out);
                        set_cols(50);
                        mid_off = Some(out.len());
                    }
                    // Stage 2 -> 3: the width dropped with no signal sent, so that was the fallback
                    // re-querying and its 1 s clock is now at zero. Shrink to 30 and signal; from
                    // here the two mechanisms are ~1 s apart.
                    (Some(off), None) if dropped_below(&out[off..], wide.unwrap()) => {
                        mid = narrowest(&out[off..]);
                        set_cols(30);
                        killpg(pgid, Signal::SIGWINCH).unwrap();
                        signalled_at = Some(Instant::now());
                        resize_off = Some(out.len());
                    }
                    // Stage 3: the narrow redraw. Stop feeding so cp reaches EOF and cprog exits
                    // — no dependence on copy speed.
                    (_, Some(off))
                        if narrow_after.is_none() && dropped_below(&out[off..], mid.unwrap()) =>
                    {
                        narrow_after = Some(signalled_at.unwrap().elapsed());
                        stop_feed.store(true, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        }
    }
    child.wait().unwrap();
    done.store(true, Ordering::Relaxed);
    stop_feed.store(true, Ordering::Relaxed);
    let _ = feeder.join();
    let _ = watchdog.join();

    assert!(!hung.load(Ordering::Relaxed), "cprog hung; relayout never produced a narrow footer");
    assert!(mid_off.is_some(), "footer never appeared, so resize was not exercised");
    assert!(resize_off.is_some(), "the unsignalled shrink never relayouted, so the fallback clock \
                                   was never zeroed and the timing below would prove nothing");
    let widths = footer_widths(&out);
    assert!(widths.iter().any(|&w| w > 40), "some wide (80-col) footers before resize: {widths:?}");
    // The relayout produced a narrow (<=35) footer *after* the SIGWINCH, proving the re-query.
    let took = narrow_after
        .unwrap_or_else(|| panic!("footer must re-lay-out to the narrow width: {widths:?}"));
    // 400 ms sits between the two explanations: a served signal relayouts on the next render tick
    // (8 ms here), while the fallback — just reset in stage 2 — cannot arrive for ~1 s.
    assert!(
        took < Duration::from_millis(400),
        "the relayout took {took:?}, which is the 1 s fallback re-query, not the SIGWINCH: {widths:?}"
    );
}

#[test]
fn an_unsized_terminal_is_laid_out_as_eighty_columns() {
    // exceptions F14 (#50, #51). `openpty` without a `Winsize` leaves the terminal reporting
    // 0×0, so `TIOCGWINSZ` gives cprog nothing to lay out against and the 80×24 initial value
    // survives — for the whole run, not just one tick, because the query is only ever retried
    // and never falls back to anything else.
    //
    // Pinned by comparison rather than by a magic number: the footer an unsized terminal gets
    // must be exactly the footer an 80-column one gets. The 40-column case is measured too, so
    // the test cannot pass by cprog ignoring the terminal width altogether.
    let widths_at = |cols: u16| -> Vec<usize> {
        let tmp = TmpDir::new(&format!("unsized{cols}"));
        let src = tmp.0.join("src.bin");
        let dst = tmp.0.join("dst.bin");
        std::fs::write(&src, vec![0u8; 200 * 1024 * 1024]).unwrap();

        // cols == 0 means "never sized": hand openpty no Winsize at all.
        let ws = (cols != 0).then_some(Winsize {
            ws_row: 24,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        });
        let pty = openpty_cloexec(ws.as_ref());
        let out_fd: OwnedFd = pty.slave.try_clone().unwrap();
        let err_fd: OwnedFd = pty.slave.try_clone().unwrap();

        let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
            .arg(&src)
            .arg(&dst)
            .env("TERM", "xterm")
            .env("LC_ALL", "C.UTF-8")
            .env_remove("CI")
            .env("CPROG_SLOW_THRESHOLD_MS", "1")
            .env("CPROG_SAMPLE_INTERVAL_MS", "5")
            .env("CPROG_RENDER_TICK_MS", "5")
            .stdin(Stdio::null())
            .stdout(Stdio::from(out_fd))
            .stderr(Stdio::from(err_fd))
            .spawn()
            .expect("spawn cprog");
        drop(pty.slave);

        let mut master = File::from(pty.master);
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match read_retry(&mut master, &mut buf) {
                0 => break,
                n => out.extend_from_slice(&buf[..n]),
            }
        }
        assert!(child.wait().unwrap().success(), "the copy still succeeds at {cols} cols");
        footer_widths(&out)
    };

    let unsized_max = *widths_at(0).iter().max().expect("a footer was drawn on an unsized pty");
    let eighty_max = *widths_at(80).iter().max().expect("a footer was drawn at 80 cols");
    let forty_max = *widths_at(40).iter().max().expect("a footer was drawn at 40 cols");

    assert_eq!(
        unsized_max, eighty_max,
        "an unsized terminal must get the 80-column layout (F14), not a wider or narrower one"
    );
    assert!(
        forty_max < eighty_max,
        "control: a terminal that does report its width is laid out to it — 40 cols gave \
         {forty_max}, 80 cols gave {eighty_max}"
    );
    assert!(eighty_max <= 80, "and the 80-column layout still fits 80 columns");
}

#[test]
fn a_lost_sigwinch_is_recovered_by_the_fallback_requery() {
    // exceptions F2's other half, and the one nothing held (#59). The rule is "a SIGWINCH that
    // never arrives must not pin the layout to a stale width" — a one-second fallback re-query.
    // `term.rs::resize_requery_rule` pins the comparison *shape* against a fallback it declares
    // itself, so `SIZE_FALLBACK` could be changed to an hour and the whole suite stayed green.
    // The sibling test above cannot see it either: it raises SIGWINCH, so the flag arm answers
    // first and the two paths are indistinguishable.
    //
    // Losing the signal for real is easy now that #60 established the mask is inherited across
    // exec: block SIGWINCH in the child before it becomes cprog, and no delivery is possible.
    // signal-hook still installs its handler; the signal simply never arrives. So a relayout
    // after the resize can only be the fallback's doing.
    let tmp = TmpDir::new("lostwinch");
    let fifo = tmp.0.join("src.fifo");
    let dst = tmp.0.join("dst.bin");
    nix::unistd::mkfifo(&fifo, nix::sys::stat::Mode::from_bits_truncate(0o600)).unwrap();

    let ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty_cloexec(Some(&ws));
    let out_fd: OwnedFd = pty.slave.try_clone().unwrap();
    let err_fd: OwnedFd = pty.slave.try_clone().unwrap();

    // The `Command` is scoped: a named one keeps its `Stdio` fds — here dup'd PTY slaves — alive
    // for a possible re-spawn, so the parent would still hold the slave and the master would
    // never see EOF. `tests/managed.rs` documents the same trap; `pre_exec` is what forces a
    // named binding here, and the block is what undoes it.
    let mut child = {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cprog"));
        cmd.arg(&fifo)
            .arg(&dst)
            .env("TERM", "xterm")
            .env("LC_ALL", "C.UTF-8")
            .env_remove("CI")
            .env("CPROG_SLOW_THRESHOLD_MS", "1")
            .env("CPROG_SAMPLE_INTERVAL_MS", "8")
            .env("CPROG_RENDER_TICK_MS", "8")
            .process_group(0)
            .stdin(Stdio::null())
            .stdout(Stdio::from(out_fd))
            .stderr(Stdio::from(err_fd));
        // SAFETY: `pthread_sigmask` is async-signal-safe and touches only this forked child.
        unsafe {
            cmd.pre_exec(|| {
                let mut set: libc::sigset_t = std::mem::zeroed();
                libc::sigemptyset(&mut set);
                libc::sigaddset(&mut set, libc::SIGWINCH);
                libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
                Ok(())
            });
        }
        cmd.spawn().unwrap()
    };
    let master_fd = pty.master.as_raw_fd();
    let cprog_pid = Pid::from_raw(child.id() as i32);
    drop(pty.slave);

    let stop_feed = Arc::new(AtomicBool::new(false));
    let feeder = {
        let (fifo, stop) = (fifo.clone(), Arc::clone(&stop_feed));
        std::thread::spawn(move || {
            let Some(mut w) = open_fifo_writer(&fifo) else { return };
            let chunk = vec![0u8; 64 * 1024];
            while !stop.load(Ordering::Relaxed) {
                if w.write_all(&chunk).is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        })
    };

    let hung = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let (hung, done) = (Arc::clone(&hung), Arc::clone(&done));
        std::thread::spawn(move || {
            for _ in 0..100 {
                std::thread::sleep(Duration::from_millis(100));
                if done.load(Ordering::Relaxed) {
                    return;
                }
            }
            hung.store(true, Ordering::Relaxed);
            let _ = kill(cprog_pid, Signal::SIGKILL);
        })
    };

    let track: [u8; 3] = [0xE2, 0x96, 0x91]; // ░
    let mut master = File::from(pty.master);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut resize_off: Option<usize> = None;
    let mut narrow_seen = false;
    loop {
        match read_retry(&mut master, &mut buf) {
            0 => break,
            n => {
                out.extend_from_slice(&buf[..n]);
                match resize_off {
                    None => {
                        if out.windows(3).any(|w| w == track) {
                            let narrow =
                                Winsize { ws_row: 24, ws_col: 30, ws_xpixel: 0, ws_ypixel: 0 };
                            // No `killpg` here — and the kernel's own SIGWINCH cannot be
                            // delivered either, because it is blocked in cprog.
                            unsafe { libc::ioctl(master_fd, libc::TIOCSWINSZ, &narrow) };
                            resize_off = Some(out.len());
                        }
                    }
                    Some(off)
                        if !narrow_seen
                            && footer_widths(&out[off..]).iter().any(|&w| w <= 30) =>
                    {
                        narrow_seen = true;
                        stop_feed.store(true, Ordering::Relaxed);
                    }
                    _ => {}
                }
            }
        }
    }
    child.wait().unwrap();
    done.store(true, Ordering::Relaxed);
    stop_feed.store(true, Ordering::Relaxed);
    let _ = feeder.join();
    let _ = watchdog.join();

    assert!(resize_off.is_some(), "footer never appeared, so the resize was not exercised");
    assert!(
        !hung.load(Ordering::Relaxed),
        "cprog hung: with SIGWINCH lost, only the fallback re-query can find the new width"
    );
    assert!(
        narrow_seen,
        "no narrow footer after a resize whose SIGWINCH never arrived — the fallback re-query \
         did not happen"
    );
}
