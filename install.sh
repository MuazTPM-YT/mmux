#!/bin/sh
# mmux installer — build from source and install the `mmux` binary.
#
# Usage:
#   ./install.sh                      # from a checkout of the repo
#   curl -fsSL https://raw.githubusercontent.com/MuazTPM-YT/mmux/main/install.sh | sh
#
# Environment:
#   MMUX_REPO    git URL to clone when not run inside the repo
#   MMUX_BINDIR  install destination (default: ~/.local/bin)
set -eu

REPO="${MMUX_REPO:-https://github.com/MuazTPM-YT/mmux}"
BINDIR="${MMUX_BINDIR:-$HOME/.local/bin}"

info() { printf '\033[36m::\033[0m %s\n' "$1"; }
err()  { printf '\033[31merror:\033[0m %s\n' "$1" >&2; exit 1; }

# 1. Toolchain -------------------------------------------------------------
if ! command -v cargo >/dev/null 2>&1; then
    err "Rust/cargo not found. Install it from https://rustup.rs then re-run this script."
fi

# 2. Source ----------------------------------------------------------------
CLEANUP=""
if [ -f Cargo.toml ] && grep -q '^name = "mmux"' Cargo.toml 2>/dev/null; then
    SRC="$(pwd)"
else
    command -v git >/dev/null 2>&1 || err "git is required to fetch the source."
    TMP="$(mktemp -d)"
    CLEANUP="$TMP"
    info "Cloning $REPO"
    git clone --depth 1 "$REPO" "$TMP/mmux"
    SRC="$TMP/mmux"
fi

# 3. Build -----------------------------------------------------------------
info "Building mmux (release, this can take a minute)…"
( cd "$SRC" && cargo build --release )

# 4. Install ---------------------------------------------------------------
mkdir -p "$BINDIR"
install -m 0755 "$SRC/target/release/mmux" "$BINDIR/mmux"
info "Installed $BINDIR/mmux"
[ -n "$CLEANUP" ] && rm -rf "$CLEANUP"

# 5. PATH ------------------------------------------------------------------
case ":$PATH:" in
    *":$BINDIR:"*) ;;
    *)
        info "Add $BINDIR to your PATH:"
        if echo "${SHELL:-}" | grep -q fish; then
            printf '     fish_add_path %s\n' "$BINDIR"
        else
            printf '     echo '\''export PATH="%s:$PATH"'\'' >> ~/.profile\n' "$BINDIR"
        fi
        ;;
esac

info "Done — start a session with:  mmux"
