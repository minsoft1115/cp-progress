#![cfg(feature = "integration")]
//! Without `-v`, cprog stays as quiet as `cp` and names the file in the footer instead (#20).
//!
//! `-v` is still injected — the slow-file timer has nothing else to time by — so the interesting
//! claim is that those lines never reach the terminal, while the timing they drive still works.
//! The footer's first row carries the destination path in their place; with `-v` that row is
//! blank, because the line above already says it.

mod common;

use common::{render_screen, Feeder, TmpDir};
use nix::pty::openpty;
use nix::sys::termios::Termios;
use std::fs::File;
use std::os::fd::{AsRawFd, OwnedFd};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn winsize(rows: u16, cols: u16) -> nix::pty::Winsize {
    nix::pty::Winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 }
}

/// Copy a slow FIFO over a PTY, optionally passing `-v`, and return everything the terminal saw.
fn run(tag: &str, pass_verbose: bool, cols: u16) -> String {
    // The tag must differ per test: tests run in parallel and `TmpDir` derives its path from
    // it, so sharing one would put two copies on the same FIFO.
    let tmp = TmpDir::new(tag);
    let fifo = tmp.path("a-rather-long-source-name.bin");
    let dst = tmp.path("destination-with-a-long-name.bin");
    let cfifo = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: a fresh path inside our own temp dir; failure is reported below.
    assert_eq!(unsafe { libc::mkfifo(cfifo.as_ptr(), 0o600) }, 0, "mkfifo");
    let feeder = Feeder::start(fifo.clone());

    let pty = openpty(Some(&winsize(24, cols)), None::<&Termios>).unwrap();
    let out_fd: OwnedFd = pty.slave.try_clone().unwrap();
    let err_fd: OwnedFd = pty.slave.try_clone().unwrap();

    let mut child = {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cprog"));
        if pass_verbose {
            cmd.arg("-v");
        }
        cmd.arg(&fifo)
            .arg(&dst)
            .env("TERM", "xterm")
            .env("LC_ALL", "C.UTF-8")
            .env("NO_COLOR", "1")
            .env("CPROG_SLOW_THRESHOLD_MS", "1")
            .env("CPROG_SAMPLE_INTERVAL_MS", "10")
            .env("CPROG_RENDER_TICK_MS", "10")
            .env_remove("CI")
            .stdin(Stdio::null())
            .stdout(Stdio::from(out_fd))
            .stderr(Stdio::from(err_fd));
        cmd.spawn().unwrap()
    };
    drop(pty.slave);

    // The FIFO keeps the copy alive for as long as it is fed, so watch until the footer is up,
    // then stop feeding: cp reaches EOF, cprog exits and the master finally sees EOF too. Reading
    // to EOF first would wait forever.
    let mut master = File::from(pty.master);
    let fd = master.as_raw_fd();
    let mut out = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    common::drain(&mut master, fd, &mut out, deadline, Some((0, "\u{2591}".as_bytes())));
    drop(feeder);

    let mut buf = [0u8; 8192];
    loop {
        match common::read_retry(&mut master, &mut buf) {
            0 => break,
            n => out.extend_from_slice(&buf[..n]),
        }
    }
    child.wait().unwrap();
    String::from_utf8_lossy(&out).into_owned()
}

#[test]
fn without_verbose_no_cp_output_reaches_the_terminal() {
    let seen = run("noverb_silent", false, 100);
    assert!(
        !seen.contains("->"),
        "cp's -v lines must not be relayed when they were not asked for: {seen:?}"
    );
    // The bar still appeared, which is only possible if the timing pulses still arrived — proof
    // the lines were read and counted rather than simply not produced.
    assert!(
        seen.contains('\u{2588}') || seen.contains('\u{2591}'),
        "the footer still engaged, so `-v` was still injected and timed: {seen:?}"
    );
}

#[test]
fn without_verbose_the_footer_names_the_destination() {
    let seen = run("noverb_name", false, 100);
    assert!(
        seen.contains("destination-with-a-long-name.bin"),
        "the footer's first row names the file being written: {seen:?}"
    );
    assert!(
        !seen.contains("a-rather-long-source-name.bin"),
        "it names the destination, not the source: {seen:?}"
    );
}

#[test]
fn with_verbose_the_lines_are_relayed_and_the_name_row_stays_blank() {
    let seen = run("verb_relay", true, 100);
    assert!(seen.contains("->"), "asked for -v, so the lines arrive: {seen:?}");
    assert!(
        !seen.contains("destination-with-a-long-name.bin\u{1b}[K"),
        "the name row is blank — the -v line above already named the file: {seen:?}"
    );
}

#[test]
fn a_narrow_terminal_never_gets_a_row_wider_than_it_is() {
    // Load-bearing rather than cosmetic: the two-row erase walks the cursor up one line, so a row
    // the terminal folds would leave every later write a line off (docs/ui.md invariant 7).
    const COLS: usize = 30;
    let seen = run("noverb_narrow", false, COLS as u16);
    for line in render_screen(seen.as_bytes()) {
        assert!(
            line.chars().count() <= COLS,
            "{:?} is {} columns wide, terminal is {COLS}",
            line,
            line.chars().count()
        );
    }
    assert!(seen.contains('…'), "the path was truncated from the front: {seen:?}");
}
