#![cfg(feature = "integration")]
//! Managed-mode integration test over a real PTY with a real `cp` (docs/testing.md B2).
//!
//! This is the set of tests that proves the live pipeline end to end: attach cprog's stdout/stderr
//! to a pseudo-terminal so it enters managed mode, copy under a tiny slow threshold, and confirm
//! from the master side that the injected `-v` lines streamed *during* the copy and the footer
//! bar was drawn — then cleared, with a `✓` summary. The multi-file test additionally proves the
//! *during* property by ordering: a bar must be drawn *between* consecutive `-v` lines, which a
//! block-buffered flush-at-end could not satisfy.

use std::fs::File;
use std::os::fd::OwnedFd;
use std::process::{Command, Stdio};

use nix::pty::{openpty, Winsize};

mod common;
use common::{read_retry, TmpDir};

#[test]
fn managed_streams_verbose_and_draws_footer_over_pty() {
    let tmp = TmpDir::new("stream");
    let src = tmp.0.join("src.bin");
    let dst = tmp.0.join("dst.bin");
    // Large enough that the copy spans several render ticks even on fast storage.
    std::fs::write(&src, vec![0u8; 256 * 1024 * 1024]).unwrap();

    let ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty(Some(&ws), None).expect("openpty");
    let slave_out: OwnedFd = pty.slave.try_clone().unwrap();
    let slave_err: OwnedFd = pty.slave.try_clone().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .arg("-v") // #20: relaying only happens when the user asked for it
        .arg(&src)
        .arg(&dst)
        .env("TERM", "xterm")
        .env("LC_ALL", "C.UTF-8")
        .env_remove("CI") // managed requires CI unset
        // Force every file to count as "slow" and sample/redraw briskly so the footer shows.
        .env("CPROG_SLOW_THRESHOLD_MS", "1")
        .env("CPROG_SAMPLE_INTERVAL_MS", "5")
        .env("CPROG_RENDER_TICK_MS", "5")
        .stdin(Stdio::null())
        .stdout(Stdio::from(slave_out))
        .stderr(Stdio::from(slave_err))
        .spawn()
        .expect("spawn cprog");

    // Parent must hold no slave fd, or the master never sees EOF.
    drop(pty.slave);

    // Drain the master as cp writes (also unblocks cp), until the slave side closes.
    let mut master = File::from(pty.master);
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match read_retry(&mut master, &mut buf) {
            0 => break,
            n => out.extend_from_slice(&buf[..n]),
        }
    }
    let status = child.wait().expect("wait cprog");

    assert!(status.success(), "cprog exited non-zero");
    assert_eq!(std::fs::read(&dst).unwrap().len(), 256 * 1024 * 1024, "copy completed");

    // A `-v` line reached the terminal at all. This does **not** show it arrived live: the
    // master is drained to EOF, so a block-buffered flush at cp exit satisfies it just as well
    // (measured with `stdbuf -i0`). Liveness is `managed_verbose_lines_interleave_with_footer_during_copy`,
    // which requires a footer redraw *between* two `-v` lines (#61).
    assert!(out.windows(2).any(|w| w == b"->"), "expected a relayed -v line");
    // The footer bar was drawn during the copy (full-block glyph, U+2588 = E2 96 88).
    assert!(
        out.windows(3).any(|w| w == [0xE2, 0x96, 0x88]),
        "expected footer bar glyphs during the copy"
    );
    // The footer was cleared and a success summary printed (✓ = E2 9C 93).
    assert!(
        out.windows(3).any(|w| w == [0xE2, 0x9C, 0x93]),
        "expected a ✓ summary on completion"
    );
}

#[test]
fn managed_verbose_lines_interleave_with_footer_during_copy() {
    // docs/testing.md B2, strengthened. The single-file test only proves a `->` exists
    // *somewhere*, which a block-buffered flush-at-end would also satisfy. Copying several files
    // proves the `-v` lines arrive *during* the copy: cp prints each `'src' -> 'dst'` line before
    // copying that file, so with live `stdbuf -oL` streaming the footer bar is redrawn *between*
    // consecutive `->` lines. If line-buffering broke, all `-v` lines would flush together at cp
    // exit (contiguous arrows, no bar between them) and this assertion fails.
    let tmp = TmpDir::new("interleave");
    let dst = tmp.0.join("out");
    std::fs::create_dir_all(&dst).unwrap();
    const N: usize = 4;
    const EACH: usize = 64 * 1024 * 1024;

    let ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty(Some(&ws), None).expect("openpty");
    let slave_out: OwnedFd = pty.slave.try_clone().unwrap();
    let slave_err: OwnedFd = pty.slave.try_clone().unwrap();

    // Scope the Command so it is dropped before draining: a named Command retains its Stdio fds
    // (dup'd PTY slave) for a possible re-spawn, which would keep the slave open in the parent
    // and the master would never see EOF. Dropping it here leaves the child as the sole holder.
    let mut child = {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cprog"));
        cmd.arg("-v"); // #20: the relay under test only runs when `-v` was actually requested
        for i in 0..N {
            let f = tmp.0.join(format!("src{i}.bin"));
            std::fs::write(&f, vec![0u8; EACH]).unwrap();
            cmd.arg(&f);
        }
        cmd.arg(&dst); // multi-source -> dest must be an existing directory
        cmd.env("TERM", "xterm")
            .env("LC_ALL", "C.UTF-8")
            .env_remove("CI")
            .env("CPROG_SLOW_THRESHOLD_MS", "1")
            .env("CPROG_SAMPLE_INTERVAL_MS", "5")
            .env("CPROG_RENDER_TICK_MS", "5")
            .stdin(Stdio::null())
            .stdout(Stdio::from(slave_out))
            .stderr(Stdio::from(slave_err))
            .spawn()
            .expect("spawn cprog")
    };
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
    let status = child.wait().expect("wait cprog");

    assert!(status.success(), "cprog exited non-zero");
    for i in 0..N {
        assert_eq!(
            std::fs::read(dst.join(format!("src{i}.bin"))).unwrap().len(),
            EACH,
            "file {i} copied fully"
        );
    }

    let first = out.windows(2).position(|w| w == b"->").expect("at least one -v line");
    let last = out.windows(2).rposition(|w| w == b"->").expect("at least one -v line");
    assert!(last > first, "multi-file copy should emit multiple -v lines");
    // A footer bar glyph (U+2588 = E2 96 88) must appear *between* the first and last `-v` line:
    // proof cprog kept rendering live between file boundaries, rather than the `-v` stream landing
    // as one block at the end (which stdbuf -oL exists to prevent).
    assert!(
        out[first..last].windows(3).any(|w| w == [0xE2, 0x96, 0x88]),
        "expected a footer redraw between streamed -v lines (live stdbuf -oL streaming)"
    );

    // Each `-v` line is relayed exactly once. The render loop drains the queue into a batch and
    // writes it in one go; dropping the `batch.clear()` between iterations re-writes everything
    // still in the batch on every wake-up, and nothing noticed — one line came out eight times
    // against two at baseline (#59). Counting is the cheapest way to see it, and it is the same
    // property a reader cares about: cp said this once, so the screen says it once.
    let text = String::from_utf8_lossy(&out);
    for i in 0..N {
        let name = format!("src{i}.bin' ->");
        assert_eq!(
            text.matches(&name).count(),
            1,
            "`{name}` must be relayed exactly once, not re-flushed with the next batch"
        );
    }
}

/// Read a PTY master to EOF.
fn drain_to_eof(master: &mut File) -> Vec<u8> {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match read_retry(master, &mut buf) {
            0 => break,
            n => out.extend_from_slice(&buf[..n]),
        }
    }
    out
}

#[test]
fn a_c_locale_copy_emits_nothing_outside_ascii() {
    // #31 / exceptions F11, as one end-to-end guard rather than a rule per glyph. On LC_ALL=C
    // every glyph position must fall back together — the bar, the eta hourglass, the name row's
    // ellipsis and the exit summary's marker. With ASCII file names cp contributes nothing
    // outside ASCII either, so a single byte above 127 anywhere in the stream is cprog's.
    //
    // Written as a byte-range assertion on purpose: a test naming today's glyphs would not catch
    // tomorrow's.
    let tmp = TmpDir::new("clocale");
    let src = tmp.0.join("src.bin");
    // Long enough that the 80-column name row has to truncate it. Without that the ellipsis —
    // one of the four glyph positions this test enumerates — is never rendered, and the
    // enumeration is a claim the run does not make: forcing `…` unconditionally in `name_row`
    // left this test green (#61).
    let dst = tmp.0.join(format!("dst_{}.bin", "x".repeat(90)));
    // The whole point is "any byte above 127 is cprog's", and the footer's name row prints the
    // destination path. A non-ASCII TMPDIR would fail this test for a reason that has nothing to
    // do with cprog, so skip rather than mislead.
    if !dst.to_string_lossy().is_ascii() {
        return;
    }
    std::fs::write(&src, vec![0u8; 256 * 1024 * 1024]).unwrap();

    let ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty(Some(&ws), None).expect("openpty");
    let slave_out: OwnedFd = pty.slave.try_clone().unwrap();
    let slave_err: OwnedFd = pty.slave.try_clone().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .arg(&src)
        .arg(&dst)
        .env("TERM", "xterm")
        .env("LC_ALL", "C") // not UTF-8 -> Style.unicode is false
        .env("LANG", "C")
        .env_remove("LC_CTYPE")
        .env_remove("CI")
        .env("CPROG_SLOW_THRESHOLD_MS", "1")
        .env("CPROG_SAMPLE_INTERVAL_MS", "5")
        .env("CPROG_RENDER_TICK_MS", "5")
        .stdin(Stdio::null())
        .stdout(Stdio::from(slave_out))
        .stderr(Stdio::from(slave_err))
        .spawn()
        .expect("spawn cprog");
    drop(pty.slave);

    let out = drain_to_eof(&mut File::from(pty.master));
    assert!(child.wait().expect("wait cprog").success());

    // The footer really did engage — otherwise this would pass by drawing nothing at all. The
    // summary is gated on that, and `[ok]` is the ASCII marker, so it proves both at once.
    // (Not matching on the bar's `[##`: the fill is wrapped in an SGR run, so `[` and `#` are
    // never adjacent in the byte stream.)
    let text = String::from_utf8_lossy(&out);
    assert!(text.contains("[ok] done"), "expected an ASCII summary after a monitored copy");
    assert!(text.contains(" % "), "expected a footer bar row");

    let stray: Vec<u8> = out.iter().copied().filter(|b| *b > 127).collect();
    assert!(
        stray.is_empty(),
        "{} non-ASCII byte(s) reached a C-locale terminal, first few: {:?}",
        stray.len(),
        &stray[..stray.len().min(12)]
    );
}

#[test]
fn the_version_notice_is_ascii_on_a_c_locale_terminal() {
    // The passthrough half of the same rule, and the easier one to miss: `--help` lays out no
    // footer, so nothing else on this path consults Style at all. The separator was an em dash
    // unconditionally, which a C-locale terminal shows as three replacement characters.
    let ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty(Some(&ws), None).unwrap();
    let out_fd: OwnedFd = pty.slave.try_clone().unwrap();
    let err_fd: OwnedFd = pty.slave.try_clone().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .arg("--help")
        .env("TERM", "xterm")
        .env("LC_ALL", "C")
        .env("LANG", "C")
        .env_remove("LC_CTYPE")
        .env_remove("CI")
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_fd))
        .stderr(Stdio::from(err_fd))
        .spawn()
        .unwrap();
    drop(pty.slave);

    let out = drain_to_eof(&mut File::from(pty.master));
    assert_eq!(child.wait().unwrap().code(), Some(0));

    let text = String::from_utf8_lossy(&out);
    let line = text
        .lines()
        .find(|l| l.contains(&format!("cprog {}", env!("CARGO_PKG_VERSION"))))
        .unwrap_or_else(|| panic!("cprog names itself on a terminal: {text:?}"));
    assert!(line.is_ascii(), "the notice must stay ASCII on a C locale: {line:?}");
}

#[test]
fn preserve_all_still_gets_the_managed_tui() {
    // #30 / exceptions B13a. `--preserve=all` is an ordinary cp invocation, but its attached
    // `=value` used to trip cprog's own argument scan, and the conservative Scan -> passthrough
    // fallback (B13) then took the progress bar away with no diagnostic. The same shape covers
    // --reflink=, --sparse=, --backup=, --no-preserve=, --update= and --context=.
    //
    // Asserting on the bar rather than the mode: `-v` is not passed, so the footer's own glyphs
    // are the only thing that distinguishes managed from a passthrough that just ran cp.
    let tmp = TmpDir::new("preserveall");
    let src = tmp.0.join("src.bin");
    let dst = tmp.0.join("dst.bin");
    std::fs::write(&src, vec![0u8; 256 * 1024 * 1024]).unwrap();

    let ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty(Some(&ws), None).expect("openpty");
    let slave_out: OwnedFd = pty.slave.try_clone().unwrap();
    let slave_err: OwnedFd = pty.slave.try_clone().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .arg("--preserve=all")
        .arg(&src)
        .arg(&dst)
        .env("TERM", "xterm")
        .env("LC_ALL", "C.UTF-8")
        .env_remove("CI")
        .env("CPROG_SLOW_THRESHOLD_MS", "1")
        .env("CPROG_SAMPLE_INTERVAL_MS", "5")
        .env("CPROG_RENDER_TICK_MS", "5")
        .stdin(Stdio::null())
        .stdout(Stdio::from(slave_out))
        .stderr(Stdio::from(slave_err))
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
    let status = child.wait().expect("wait cprog");

    assert!(status.success(), "cprog exited non-zero");
    assert_eq!(std::fs::read(&dst).unwrap().len(), 256 * 1024 * 1024, "copy completed");
    assert!(
        out.windows(3).any(|w| w == [0xE2, 0x96, 0x88]),
        "expected footer bar glyphs: --preserve=all must not drop cprog to passthrough"
    );
    // Without `-v` the footer's first row is the only thing naming the file, so it must be there.
    assert!(
        String::from_utf8_lossy(&out).contains("dst.bin"),
        "the footer's name row should still name the destination"
    );
}

#[test]
fn managed_relays_cp_error_and_preserves_exit_code() {
    // docs/testing.md D1: a cp that fails keeps its exit code, its error reaches the terminal,
    // and no summary is invented for a run that never showed a bar.
    //
    // Deliberately *not* claimed: that any of this is managed-mode behaviour. cp fails instantly,
    // so no footer, no log-area relay and no `✗` ever occur, and every assertion below holds
    // under `CPROG_PASSTHROUGH=1` too (measured). The header used to promise "a ✗ summary" while
    // the last assertion denies it; C2's relay-and-erase半 is pinned by the units and by
    // `managed_verbose_lines_interleave_with_footer_during_copy` (#61).
    let tmp = TmpDir::new("cperr");
    let src = tmp.0.join("src.bin");
    std::fs::write(&src, b"hi").unwrap();
    let bad_dst = tmp.0.join("no-such-dir").join("dst.bin"); // parent dir does not exist

    let ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty(Some(&ws), None).unwrap();
    let out_fd: OwnedFd = pty.slave.try_clone().unwrap();
    let err_fd: OwnedFd = pty.slave.try_clone().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .arg(&src)
        .arg(&bad_dst)
        .env("TERM", "xterm")
        .env("LC_ALL", "C.UTF-8")
        .env_remove("CI")
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_fd))
        .stderr(Stdio::from(err_fd))
        .spawn()
        .unwrap();
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
    let status = child.wait().unwrap();

    assert_eq!(status.code(), Some(1), "cprog returns cp's exit code");
    let text = String::from_utf8_lossy(&out);
    assert!(
        text.contains("No such file") || text.contains("cannot create"),
        "cp's error was relayed: {text:?}"
    );
    // cp fails instantly, so no footer ever engaged -> the general gate suppresses the summary
    // (cp's own error above already explains it).
    assert!(!text.contains('✗'), "no summary when nothing was monitored: {text:?}");
}

#[test]
fn help_over_pty_passes_through_but_names_cprog() {
    // `--help` prints and exits without copying; even in a terminal cprog must pass through —
    // no footer, no `✓ done` summary (bug2/#2). Over a PTY managed would otherwise engage.
    // It does append one line naming itself, because `--version`/`--help` reach `cp` untouched
    // and would otherwise leave no trace of the wrapper at all (#15).
    let ws = Winsize { ws_row: 24, ws_col: 80, ws_xpixel: 0, ws_ypixel: 0 };
    let pty = openpty(Some(&ws), None).unwrap();
    let out_fd: OwnedFd = pty.slave.try_clone().unwrap();
    let err_fd: OwnedFd = pty.slave.try_clone().unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_cprog"))
        .arg("--help")
        .env("TERM", "xterm")
        .env("LC_ALL", "C.UTF-8")
        .env_remove("CI")
        .stdin(Stdio::null())
        .stdout(Stdio::from(out_fd))
        .stderr(Stdio::from(err_fd))
        .spawn()
        .unwrap();
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
    let status = child.wait().unwrap();
    let text = String::from_utf8_lossy(&out);

    assert_eq!(status.code(), Some(0));
    assert!(text.contains("Usage: cp"), "cp's help was shown: {:.80?}", text);
    assert!(!text.contains('✓'), "no summary for --help: {text:?}");
    // The version line is the last thing on screen, after cp has had its say.
    let line = format!("cprog {}", env!("CARGO_PKG_VERSION"));
    assert!(text.contains(&line), "cprog names itself on a terminal: {text:?}");
    assert!(
        text.rfind(&line) > text.rfind("Usage: cp"),
        "the line comes after cp's output, not before it: {text:?}"
    );
}
