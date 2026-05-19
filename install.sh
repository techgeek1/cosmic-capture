#!/usr/bin/env bash
# Build cosmic-capture in release mode and install the binary + .desktop entry.
# Defaults to a user-local install under $HOME/.local; pass --prefix /usr/local
# (with sudo) for a system-wide install.

set -euo pipefail

PREFIX="${HOME}/.local"
SKIP_BUILD=0
SKIP_CHECK=0

usage() {
    cat <<EOF
Usage: $0 [options]

Options:
  --prefix DIR     Install prefix (default: \$HOME/.local).
                   Binary goes to <prefix>/bin, .desktop to
                   <prefix>/share/applications.
  --skip-build     Don't run cargo build; assume the release binary
                   already exists in ./target/release/.
  --skip-check     Don't probe gst-inspect-1.0 for required plugins.
  -h, --help       Show this message.
EOF
}

while [[ $# -gt 0 ]]; do
    case "$1" in
        --prefix)      PREFIX="$2"; shift 2 ;;
        --skip-build)  SKIP_BUILD=1; shift ;;
        --skip-check)  SKIP_CHECK=1; shift ;;
        -h|--help)     usage; exit 0 ;;
        *) echo "unknown option: $1" >&2; usage >&2; exit 1 ;;
    esac
done

REPO_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$REPO_DIR"

# --- Runtime plugin probe -----------------------------------------------------
# Bail loudly if the gst plugins we rely on aren't installed. The binary
# would link and start fine without them, then fail at first capture with
# an opaque "element X not available" error — this surfaces the missing
# pieces up front.
if [[ "$SKIP_CHECK" -eq 0 ]]; then
    if ! command -v gst-inspect-1.0 >/dev/null 2>&1; then
        echo "warning: gst-inspect-1.0 not found; can't probe gst plugins." >&2
        echo "         Install GStreamer (see README) or re-run with --skip-check." >&2
    else
        missing=()
        for el in vapostproc vah264enc vacompositor pulsesrc avenc_aac h264parse mp4mux; do
            if ! gst-inspect-1.0 --exists "$el"; then
                missing+=("$el")
            fi
        done
        if [[ ${#missing[@]} -gt 0 ]]; then
            echo "error: missing gstreamer elements: ${missing[*]}" >&2
            echo "       Install gst-plugins-{base,good,bad,ugly}, gst-libav, and" >&2
            echo "       pipewire-pulse — see the Requirements section in README.md." >&2
            exit 1
        fi
    fi
fi

# --- Build --------------------------------------------------------------------
if [[ "$SKIP_BUILD" -eq 0 ]]; then
    if ! command -v cargo >/dev/null 2>&1; then
        echo "error: cargo not found. Install Rust 1.80+ (https://rustup.rs)." >&2
        exit 1
    fi
    echo ">> cargo build --release"
    cargo build --release
fi

BIN_SRC="$REPO_DIR/target/release/cosmic-capture"
if [[ ! -x "$BIN_SRC" ]]; then
    echo "error: $BIN_SRC not found (or not executable)." >&2
    echo "       Build first, or drop --skip-build." >&2
    exit 1
fi

# --- Install ------------------------------------------------------------------
BIN_DIR="$PREFIX/bin"
DESK_DIR="$PREFIX/share/applications"

mkdir -p "$BIN_DIR" "$DESK_DIR"

install -m 0755 "$BIN_SRC" "$BIN_DIR/cosmic-capture"
install -m 0644 "$REPO_DIR/assets/cosmic-capture.desktop" "$DESK_DIR/cosmic-capture.desktop"

echo ">> installed:"
echo "    $BIN_DIR/cosmic-capture"
echo "    $DESK_DIR/cosmic-capture.desktop"

# Refresh the desktop-entry cache so the launcher picks it up immediately.
# Non-fatal if missing (some minimal installs don't ship it).
if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DESK_DIR" >/dev/null 2>&1 || true
fi

# PATH check — easy to miss if $HOME/.local/bin isn't on PATH yet.
if [[ ":$PATH:" != *":$BIN_DIR:"* ]]; then
    echo
    echo "note: $BIN_DIR is not on your PATH."
    echo "      Add this to your shell's rc file:"
    echo "          export PATH=\"$BIN_DIR:\$PATH\""
fi
