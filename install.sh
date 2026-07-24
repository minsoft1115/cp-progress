#!/bin/sh
# cprog installer — build & install cprog, then wire up `alias cp='cprog'` for your shell.
#
#   curl -fsSL https://raw.githubusercontent.com/minsoft1115/cp-progress/main/install.sh | sh
#
# Env knobs:
#   CPROG_NO_ALIAS=1       install only; do not add the `cp` alias
#   CPROG_REPO=user/repo   override the source repo (default: minsoft1115/cp-progress)
set -eu

REPO="${CPROG_REPO:-minsoft1115/cp-progress}"
CARGO_BIN="${CARGO_HOME:-$HOME/.cargo}/bin"

info() { printf '\033[1;32m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m  %s\n' "$*" >&2; }
die()  { printf '\033[1;31mxx\033[0m  %s\n' "$*" >&2; exit 1; }

# --- prerequisites --------------------------------------------------------------
[ "$(uname -s)" = "Linux" ] || warn "The progress bar is Linux-only; elsewhere cprog behaves exactly like cp (passthrough)."

command -v cp >/dev/null 2>&1 || die "System 'cp' not found (coreutils is required)."
command -v stdbuf >/dev/null 2>&1 || warn "'stdbuf' (coreutils) not found — cprog will run as passthrough, without the progress bar."

command -v cargo >/dev/null 2>&1 || die "Rust (cargo) is required. Install it first:
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
then run 'source \"\$HOME/.cargo/env\"' and re-run this script."

# --- build & install ------------------------------------------------------------
info "Installing cprog — cargo install --git https://github.com/$REPO"
cargo install --git "https://github.com/$REPO" --locked --force

[ -x "$CARGO_BIN/cprog" ] || die "cprog was not found at $CARGO_BIN/cprog after install. Check the cargo output above."
info "Installed: $CARGO_BIN/cprog"

# --- shell integration (PATH + alias) -------------------------------------------
BEGIN="# >>> cprog >>>"
END="# <<< cprog <<<"

rc_for_shell() {
    case "$(basename "${SHELL:-}")" in
        zsh)  printf '%s\n' "${ZDOTDIR:-$HOME}/.zshrc" ;;
        bash) printf '%s\n' "$HOME/.bashrc" ;;
        *)    printf '%s\n' "" ;;
    esac
}

RC="$(rc_for_shell)"

if [ -z "$RC" ]; then
    warn "Could not detect your shell (\$SHELL=${SHELL:-unset}). Add this to your shell rc by hand:"
    printf '    export PATH="%s:$PATH"\n' "$CARGO_BIN"
    [ "${CPROG_NO_ALIAS:-0}" = "1" ] || printf "    alias cp='cprog'\n"
elif [ -f "$RC" ] && grep -qF "$BEGIN" "$RC"; then
    info "$RC already has a cprog block — leaving it as is."
else
    {
        printf '\n%s\n' "$BEGIN"
        printf 'export PATH="%s:$PATH"\n' "$CARGO_BIN"
        [ "${CPROG_NO_ALIAS:-0}" = "1" ] || printf "alias cp='cprog'\n"
        printf '%s\n' "$END"
    } >> "$RC"
    if [ "${CPROG_NO_ALIAS:-0}" = "1" ]; then
        info "Added PATH to $RC (alias skipped via CPROG_NO_ALIAS=1)."
    else
        info "Added PATH and 'alias cp=cprog' to $RC."
    fi
fi

# --- done -----------------------------------------------------------------------
info "Done. Open a new terminal, or apply now:"
if [ -n "$RC" ]; then
    printf '    source "%s"\n' "$RC"
fi
printf '\nCopy a large file in an interactive terminal to see the bar:\n    cp big.iso /mnt/backup/big.iso\n'
printf '(pipes / non-TTY / CI / non-Linux / no stdbuf / -i are byte-identical to cp)\n'
