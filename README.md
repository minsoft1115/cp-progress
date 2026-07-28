# cprog

[![CI](https://github.com/minsoft1115/cp-progress/actions/workflows/ci.yml/badge.svg)](https://github.com/minsoft1115/cp-progress/actions/workflows/ci.yml)
[![crates.io](https://img.shields.io/crates/v/cp-progress.svg)](https://crates.io/crates/cp-progress)
[![docs.rs](https://img.shields.io/docsrs/cp-progress)](https://docs.rs/cp-progress)
[![license](https://img.shields.io/crates/l/cp-progress.svg)](./LICENSE)
[![MSRV](https://img.shields.io/crates/msrv/cp-progress)](https://www.rust-lang.org)

**English** | [한국어](README.ko.md)

`cprog` is a thin wrapper around the system `cp`. It overlays a per-file progress bar **only in a
Linux interactive terminal** (the progress feature is Linux-only, needing `/proc`); anywhere a
footer would not be safe — a pipe, a redirect, CI, non-Linux, a background job — it behaves
**transparently, byte-for-byte identical to `cp`**. It runs the real `cp` and draws a two-row
footer **only for files that take a while**, erasing it when done. `cp -v` output is relayed only
if you asked for `-v`; otherwise cprog stays as quiet as `cp` and names the file in the footer
instead. No external `progress` command, no hidden PTY, no screen-scraping.

```
  …/backup/2026/win11-vm/win11.qcow2
  ████████████░░░░░░░░   62.33 %  0.9/1.4 GiB  (142 MiB/s)  ⏳ 00:05
```

## What it is

- The real copy is done by `cp`, and its semantics are untouched. `cp`'s exit code is the final
  authority.
- In managed mode it injects and captures `-v` for its timing — but relays it to the terminal
  **only when you passed `-v` yourself**. Without it cprog prints nothing cp would not have
  printed. When a single file gets slow, it locates that file via `/proc/<pid>/fd` and reads the
  growing size with `stat` to draw its **own progress bar**, naming the file on the row above.
- Where a footer isn't safe it falls back to passthrough — streams inherited, environment
  untouched, **byte-identical to `cp`**. That covers: stdout or stderr not a TTY (a pipe or a
  redirect), the two not on the same terminal, `TERM` unset or `dumb`, `CI` set, non-Linux,
  `stdbuf` missing, a background job (`cprog … &`), an interactive flag (`-i`),
  `--help`/`--version`, and `CPROG_PASSTHROUGH` set — an explicit escape hatch that also makes
  cprog exec the real `cp` in place, leaving no wrapper process at all.

It computes progress itself from `cp`'s own `-v` timing and the kernel's `/proc`/`stat` — no
external progress tool, no hidden PTY, no screen-scraping.

## Escape hatch

Set `CPROG_PASSTHROUGH` (any value) and cprog gets out of the way entirely: passthrough is
forced over every other condition, nothing is added — not even the `--version` line — and cprog
**execs the real `cp` in place**, so `$!`, signals and exit codes are cp's own, with no wrapper
process left.

```bash
CPROG_PASSTHROUGH=1 cp -r photos backup   # this one copy, exactly plain cp
export CPROG_PASSTHROUGH=1                # this whole shell (unset to restore)
```

To bypass cprog altogether there is always `\cp` (or `command cp`), which skips the alias.

## Why cprog — vs. the alternatives

"Show cp's progress" already has answers. The question is: **is it still the real `cp`, and what
does the progress cost you?**

| | `progress` | `advcpmv` | `cpx` | `rsync` | **cprog** |
|---|---|---|---|---|---|
| Still the real `cp`? | ✓ watches the real cp from outside | △ GNU cp with a progress patch, built yourself | ✗ its own implementation (Rust) | ✗ a different tool | ✓ wraps the real cp |
| How you invoke it | a second command alongside the copy (`progress -mp $!` / `watch progress`) | `advcp -g …` (or alias it over `cp`) | `cpx …` (its own command) | `rsync -a --info=progress2 …` | `cprog <cp args…>` — arguments are cp's own; the installer aliases `cp` to it |
| Install | distro package | patch + recompile coreutils | `cargo install` / AUR | usually preinstalled | `cargo install` / one-liner |
| Tracks latest coreutils | ✓ nothing to port — observes whatever cp runs | pinned to the coreutils release the patch targets | n/a (own code) | n/a | ✓ rides the system cp — always current |
| Risk of changing cp's behavior | none | low — GNU cp, one patch | possible — an independent implementation | different semantics by design | none — runs your real cp |
| Progress accuracy | estimated from outside (fd positions — not every copy path updates them) | high | high | high | approximate, per-file |

What the table says — different tools for different jobs:

- **`progress`** — closest in spirit: it watches your real cp from the outside and installs
  nothing into the copy path. The costs are a second command to run, and an outside view that
  can only read what the kernel exposes — which not every copy path updates. (cprog keeps the
  observer idea and reads the destination's growing size instead —
  [`docs/progress-model.md`](./docs/progress-model.md).)
- **`advcpmv`** — the most accurate bar `cp` itself can give, because it *is* GNU cp with a
  progress patch. The costs are compiling coreutils yourself and living on the release the
  patch targets.
- **`cpx`** — a modern reimplementation with a polished bar and speed as an explicit goal. The
  cost is that it is its own tool: `cp`'s exact semantics are not part of its contract.
- **`rsync`** — powerful, ubiquitous, and the right answer for syncing. It simply answers a
  different question than `cp`, with argument and preservation semantics of its own.
- **cprog** — **just `cp` with a progress bar**: light install, always the current system
  cp, behavior unchanged. The cost: the bar is approximate.

**Honest trade-off:** cprog's progress is a per-file estimate (no whole-operation %/ETA) and only
appears in a Linux interactive terminal with `stdbuf`; for raw bar accuracy, `advcpmv`, `cpx`, and
`rsync` are ahead. cprog sells **"exactly `cp`, with no friction"** — not the fanciest bar.

## Design decisions (in brief)

- **It does not count files** — counting "files only" would need a `stat` per entry, hurting
  performance on many small files. `-v` is used only as an activity signal and for slow-file
  timing; its **contents are never parsed**.
- Progress is measured from the **destination file's size**, not `fdinfo: pos` — because with
  coreutils 9.x's `copy_file_range`, `pos` stays 0 for the whole copy.
- Always its `st_size`, never the block count. A **sparse** destination — which `cp` produces by
  default via `--sparse=auto` whenever the source has holes — has far fewer blocks than its
  length, as do compressing filesystems and ext4 mid-writeback; counting blocks would leave the
  bar short of 100% in all three. See [`docs/progress-model.md`](./docs/progress-model.md).

## Status

**Implemented (test-first).** The design was fixed docs-first in [`docs/`](./docs), then built
from that spec with TDD. Both passthrough (byte-identical to `cp`) and the managed TUI (live
footer) work; pure unit tests plus PTY integration tests verify the core contracts (preserving
`cp`'s result and signals, byte-identical fallback, live streaming).

```bash
cargo test                          # unit suite — needs no external tools, always green
cargo test --features integration   # adds the PTY tests driving a real cp / stdbuf
```

The integration tests are feature-gated on purpose so the default `cargo test` stays free of
external dependencies; run both before trusting a change.

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
- [Testing](./docs/testing.md) · [Usage](./docs/usage.md) · [Dependencies](./docs/dependencies.md)
- [Performance](./docs/performance.md) — measured overhead baseline, and how it is measured
- [Exceptions](./docs/exceptions.md) — every runtime exception (signals, Ctrl-Z, passthrough
  triggers, progress limits), what cprog does about each, and where it is tested

## Requirements

- The system `cp` (required — cprog wraps it everywhere).
- **To see the progress bar:** Linux (needs `/proc`) + an interactive terminal + `stdbuf`
  (coreutils, to stream `cp -v` live). If any of these is missing, cprog automatically runs as
  passthrough (byte-identical to `cp`), and the copy still works normally.

## Tuning

All optional, all with safe defaults; an unparsable value silently falls back to the default.

| Variable | Effect |
|---|---|
| `CPROG_PASSTHROUGH` | Forces passthrough unconditionally (any value) — every cprog addition is off, version line included; cprog execs the real `cp` in place |
| `CPROG_SLOW_THRESHOLD_MS` | How long one file must take before its bar appears (default 100) |
| `CPROG_SAMPLE_INTERVAL_MS` | `stat` polling interval while a file is slow (default 100) |
| `CPROG_RENDER_TICK_MS` | Footer redraw tick (default 125) |
| `NO_COLOR` | Disables footer colour (any value) |

Full description in [`docs/usage.md`](./docs/usage.md).

## License

[MIT](./LICENSE)
