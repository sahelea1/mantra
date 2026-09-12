#!/bin/sh
# Mantra installer.
#
#   curl -fsSL https://raw.githubusercontent.com/sahelea1/mantra/master/install.sh | sh
#   wget -qO- https://raw.githubusercontent.com/sahelea1/mantra/master/install.sh | sh
#
# Tries a prebuilt release binary first (Linux x86_64/aarch64, macOS arm64/x86_64),
# verifies its sha256, and falls back to building from source with cargo. Never uses
# sudo; installs to a per-user directory and adds it to PATH if needed.
#
# Environment overrides:
#   MANTRA_INSTALL_DIR   where to install (default: $HOME/.local/bin)
#   MANTRA_VERSION       release tag to install, e.g. v0.2.0 (default: latest)
#   MANTRA_FROM_SOURCE   set to 1 to skip release assets and always build from source
#   MANTRA_REPO_URL      (undocumented, for testing) git URL to clone for source builds
#
# POSIX sh only — no bashisms. Idempotent: re-running upgrades an existing install.

set -e

REPO="sahelea1/mantra"
REPO_URL="${MANTRA_REPO_URL:-https://github.com/${REPO}.git}"
INSTALL_DIR="${MANTRA_INSTALL_DIR:-$HOME/.local/bin}"
VERSION="${MANTRA_VERSION:-latest}"
FROM_SOURCE="${MANTRA_FROM_SOURCE:-0}"

TMP_DIRS=""

log() {
    printf '%s\n' "$*"
}

err() {
    printf 'install.sh: error: %s\n' "$*" >&2
}

cleanup() {
    for d in $TMP_DIRS; do
        rm -rf "$d"
    done
}
trap cleanup EXIT INT TERM

# Prints a fresh temp dir. NOTE: always call as `d=$(new_tmpdir); TMP_DIRS="$TMP_DIRS $d"`
# on its own line — command substitution runs in a subshell, so TMP_DIRS cannot be
# updated *inside* this function and read back by the parent shell's trap.
new_tmpdir() {
    mktemp -d 2>/dev/null || mktemp -d -t mantra
}

# --- pick a downloader -------------------------------------------------

if command -v curl >/dev/null 2>&1; then
    DL=curl
elif command -v wget >/dev/null 2>&1; then
    DL=wget
else
    err "neither curl nor wget is installed; install one of them and re-run."
    exit 1
fi

fetch() {
    # fetch <url> <output-file>  — returns non-zero on any HTTP or network error.
    url="$1"
    out="$2"
    if [ "$DL" = curl ]; then
        curl -fsSL "$url" -o "$out"
    else
        wget -q "$url" -O "$out"
    fi
}

# --- detect platform -----------------------------------------------------

os_raw=$(uname -s)
arch_raw=$(uname -m)
asset=""

case "$os_raw" in
    Linux)
        case "$arch_raw" in
            x86_64 | amd64) asset="mantra-linux-x86_64" ;;
            aarch64 | arm64) asset="mantra-linux-aarch64" ;;
        esac
        ;;
    Darwin)
        case "$arch_raw" in
            arm64) asset="mantra-macos-arm64" ;;
            x86_64) asset="mantra-macos-x86_64" ;;
        esac
        ;;
esac

use_source=0
if [ "$FROM_SOURCE" = "1" ]; then
    use_source=1
    log "MANTRA_FROM_SOURCE=1 set; building from source."
elif [ -z "$asset" ]; then
    use_source=1
    log "No prebuilt binary for $os_raw/$arch_raw; building from source."
fi

binary_path=""

# --- try a release asset --------------------------------------------------

if [ "$use_source" -eq 0 ]; then
    if [ "$VERSION" = "latest" ]; then
        base_url="https://github.com/$REPO/releases/latest/download"
        log "Looking for the latest release asset ($asset)..."
    else
        base_url="https://github.com/$REPO/releases/download/$VERSION"
        log "Looking for $asset in release $VERSION..."
    fi

    dl_dir=$(new_tmpdir)
    TMP_DIRS="$TMP_DIRS $dl_dir"
    if fetch "$base_url/$asset" "$dl_dir/$asset" && fetch "$base_url/$asset.sha256" "$dl_dir/$asset.sha256"; then
        log "Downloaded $asset; verifying checksum..."
        expected=$(awk '{print $1}' "$dl_dir/$asset.sha256")
        if command -v sha256sum >/dev/null 2>&1; then
            actual=$(sha256sum "$dl_dir/$asset" | awk '{print $1}')
        elif command -v shasum >/dev/null 2>&1; then
            actual=$(shasum -a 256 "$dl_dir/$asset" | awk '{print $1}')
        else
            err "no sha256sum or shasum found; cannot verify the download. Falling back to building from source."
            actual=""
            expected="unverifiable"
        fi
        if [ "$expected" = "$actual" ] && [ -n "$actual" ]; then
            log "Checksum OK."
            chmod +x "$dl_dir/$asset"
            binary_path="$dl_dir/$asset"
        else
            err "checksum mismatch for $asset (expected $expected, got ${actual:-none}); falling back to building from source."
        fi
    else
        log "No release asset available (or download failed); falling back to building from source."
    fi
fi

# --- build from source -----------------------------------------------------

if [ -z "$binary_path" ]; then
    if ! command -v git >/dev/null 2>&1; then
        err "git is required to build from source but was not found. Install git and re-run."
        exit 1
    fi

    if ! command -v cargo >/dev/null 2>&1; then
        log "cargo not found; installing rustup (minimal profile, non-interactive)..."
        rustup_tmp=$(new_tmpdir)
        TMP_DIRS="$TMP_DIRS $rustup_tmp"
        rustup_sh="$rustup_tmp/rustup-init.sh"
        if [ "$DL" = curl ]; then
            curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs -o "$rustup_sh"
        else
            wget -q https://sh.rustup.rs -O "$rustup_sh"
        fi
        sh "$rustup_sh" -y --profile minimal
        rm -f "$rustup_sh"
        if [ -f "$HOME/.cargo/env" ]; then
            # shellcheck source=/dev/null
            . "$HOME/.cargo/env"
        fi
        if ! command -v cargo >/dev/null 2>&1; then
            err "cargo still not found after installing rustup; open a new shell and re-run this installer."
            exit 1
        fi
    fi

    src_dir=$(new_tmpdir)
    TMP_DIRS="$TMP_DIRS $src_dir"
    log "Cloning $REPO_URL..."
    if [ "$VERSION" = "latest" ]; then
        git clone --depth 1 "$REPO_URL" "$src_dir/mantra"
    else
        if ! git clone --depth 1 --branch "$VERSION" "$REPO_URL" "$src_dir/mantra" 2>/dev/null; then
            git clone "$REPO_URL" "$src_dir/mantra"
            (cd "$src_dir/mantra" && git checkout "$VERSION")
        fi
    fi

    log "Building mantra (cargo build --release)... this can take a few minutes."
    (cd "$src_dir/mantra/mantra_src" && cargo build --release)
    binary_path="$src_dir/mantra/mantra_src/target/release/mantra"
fi

if [ ! -f "$binary_path" ]; then
    err "no mantra binary was produced; aborting."
    exit 1
fi

# --- install ----------------------------------------------------------------

mkdir -p "$INSTALL_DIR"
cp "$binary_path" "$INSTALL_DIR/mantra"
chmod +x "$INSTALL_DIR/mantra"
log "Installed mantra to $INSTALL_DIR/mantra"

# --- PATH ---------------------------------------------------------------

case ":$PATH:" in
    *":$INSTALL_DIR:"*) on_path=1 ;;
    *) on_path=0 ;;
esac

if [ "$on_path" -eq 0 ]; then
    case "$SHELL" in
        */zsh) rc_file="$HOME/.zshrc" ;;
        */bash) rc_file="$HOME/.bashrc" ;;
        *) rc_file="$HOME/.profile" ;;
    esac

    export_line="export PATH=\"$INSTALL_DIR:\$PATH\""

    if [ -f "$rc_file" ] && grep -Fq "$export_line" "$rc_file" 2>/dev/null; then
        log "$INSTALL_DIR is already on PATH via $rc_file."
    else
        printf '\n# added by mantra install.sh\n%s\n' "$export_line" >>"$rc_file"
        log "Added this line to $rc_file:"
        log "  $export_line"
        log "Open a new shell (or run: . $rc_file) so 'mantra' is on your PATH."
    fi
    PATH="$INSTALL_DIR:$PATH"
    export PATH
else
    log "$INSTALL_DIR is already on PATH."
fi

# --- doctor -------------------------------------------------------------

log ""
log "Running mantra doctor..."
set +e
"$INSTALL_DIR/mantra" doctor
doctor_status=$?
set -e
if [ "$doctor_status" -ne 0 ]; then
    log ""
    log "mantra doctor reported issues above (exit $doctor_status). Installation itself succeeded."
fi

exit 0
