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

## Why cprog — vs. the alternatives

"Show cp's progress" already has answers. The question is: **is it still the real `cp`, and what
does the progress cost you?**

| | `progress` | `advcpmv` | `cpx` | `rsync` | **cprog** |
|---|---|---|---|---|---|
| Still the real `cp`? | ✓ watches real cp | △ patched cp fork | ✗ Rust rewrite | ✗ different tool | ✓ wraps real cp |
| How you invoke it | a **separate command** each time (`progress -mp …` / `watch progress`) | `advcp -g …` (patched binary) | `cpx …` (new command) | `rsync -a --info=progress2 …` | just `cp …` (alias) |
| Install | distro package | **recompile coreutils** | cargo install | usually preinstalled | `cargo install` / one-liner |
| Tracks latest coreutils | n/a | **lags** (newest patch is 9.7 vs coreutils 9.10) | n/a (own code) | n/a | rides system cp — always current |
| Risk of changing cp's behavior | none | patched / old fork | **reimplementation may differ** | **rsync semantics differ** | none — runs your real cp |
| Progress accuracy | `pos`-based (weak on reflink/network) | high | high | high | approximate, per-file |

What the table says:

- **`progress`** — real cp, but every copy needs an **extra command**; it isn't integrated.
- **`advcpmv`** — *is* cp, but a **recompiled fork that lags upstream**: its newest patch targets
  coreutils 9.7 while releases are already at 9.10+.
- **`cpx` / `rsync`** — **not `cp`** at all (a reimplementation / a different tool), so behavior
  can differ (`rsync`'s trailing-slash rule and attribute defaults especially).
- **cprog** — it's **just `cp` with a progress bar**: light install, always the current system
  cp, behavior unchanged. The only cost is that the bar is approximate.

**Honest trade-off:** cprog's progress is a per-file estimate (no whole-operation %/ETA) and only
appears in a Linux interactive terminal with `stdbuf`; for raw bar accuracy, `advcpmv`, `cpx`, and
`rsync` are ahead. cprog sells **"exactly `cp`, with no friction"** — not the fanciest bar.

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

From [crates.io](https://crates.io/crates/cp-progress) (installs the `cprog` binary):

```bash
cargo install cp-progress --locked
echo "alias cp='cprog'" >> ~/.bashrc && source ~/.bashrc   # ~/.zshrc for zsh
```

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
