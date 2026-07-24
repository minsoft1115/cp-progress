# cprog

**English** | [한국어](README.ko.md)

`cprog` is a thin wrapper around the system `cp`. It overlays a per-file progress bar **only in a
Linux interactive terminal** (the progress feature is Linux-only, needing `/proc`); everywhere
else — pipes, non-TTY, CI, non-Linux — it behaves **transparently, byte-for-byte identical to
`cp`**. It runs the real `cp`, relays `cp -v` output above, and draws a footer progress bar
**only for files that take a while**, erasing it when done. No external `progress` command, no
hidden PTY, no screen-scraping.

```
  'a.iso' -> '/mnt/backup/a.iso'
  ████████░░░░  62.34 %  0.9/1.4 GiB  (142 MiB/s)  ⏳ 00:05
```

## What it is

- The real copy is done by `cp`, and its semantics are untouched. `cp`'s exit code is the final
  authority.
- In managed mode it injects and captures `-v` to relay the log above (that scrolling is the
  "it's alive" signal); when a single file gets slow, it locates it via `/proc/<pid>/fd` and
  reads the growing size with `stat` to draw its **own progress bar**.
- Where a footer isn't safe (pipe / non-TTY / CI / non-Linux), it is byte-identical to `cp`.

It computes progress itself from `cp`'s own `-v` timing and the kernel's `/proc`/`stat` — no
external progress tool, no hidden PTY, no screen-scraping.

## Design decisions (in brief)

- **It does not count files** — counting "files only" would need a `stat` per entry, hurting
  performance on many small files. `-v` is used only as an activity signal and for slow-file
  timing; its **contents are never parsed**.
- Progress is measured by the **destination file size (`stat().st_size`)**, not `fdinfo: pos` —
  because with coreutils 9.x's `copy_file_range`, `pos` stays 0.

## Status

**Implemented (test-first).** The design was fixed docs-first in [`docs/`](./docs), then built
from that spec with TDD. Both passthrough (byte-identical to `cp`) and the managed TUI (live
footer) work; pure unit tests plus PTY integration tests verify the core contracts (preserving
`cp`'s result and signals, byte-identical fallback, live streaming). Run the full suite with
`cargo test`.

## Install

One-line install (builds + wires up the `cp` alias automatically, detects bash/zsh):

```bash
curl -fsSL https://raw.githubusercontent.com/minsoft1115/cp-progress/main/install.sh | sh
```

- Requires Rust (`cargo`) (if missing, the script points you to [rustup](https://rustup.rs)). It
  uses edition 2024, so **Rust 1.85 or newer** is required (run `rustup update` if older).
- Adds `~/.cargo/bin` to your PATH and appends `alias cp='cprog'` to your shell rc
  (`.bashrc`/`.zshrc`).
- Install without the alias: `... | CPROG_NO_ALIAS=1 sh`.

Manual install:

```bash
cargo install --git https://github.com/minsoft1115/cp-progress --locked --force
echo "alias cp='cprog'" >> ~/.bashrc && source ~/.bashrc   # ~/.zshrc for zsh
```

After installing, in an interactive terminal:

```bash
cp big.iso /mnt/backup/big.iso   # a progress bar appears if it gets slow
```

## Documentation

> The docs are written in Korean — they are the authoritative design spec.

- [Docs index](./docs/index.md)
- [Overview](./docs/overview.md) · [UI](./docs/ui.md) · [Capture & Verbose](./docs/capture-and-verbose.md)
- [Progress model](./docs/progress-model.md) · [Runtime model](./docs/runtime-model.md)
- [Architecture](./docs/architecture.md) · [Process model](./docs/process-model.md)
- [Testing](./docs/testing.md) · [Usage](./docs/usage.md)

## Requirements

- The system `cp` (required — cprog wraps it everywhere).
- **To see the progress bar:** Linux (needs `/proc`) + an interactive terminal + `stdbuf`
  (coreutils, to stream `cp -v` live). If any of these is missing, cprog automatically runs as
  passthrough (byte-identical to `cp`), and the copy still works normally.
