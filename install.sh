#!/bin/sh
# Mantra installer.
#
#   curl -fsSL https://raw.githubusercontent.com/sahelea1/mantra/master/install.sh | sh
#   wget -qO- https://raw.githubusercontent.com/sahelea1/mantra/master/install.sh | sh
#
# Installs two binaries side by side: `mantra`, and `bija`, its built-in agent harness (Mantra
# finds a `bija` next to its own executable). Tries the prebuilt release binaries first (Linux
# x86_64/aarch64, macOS arm64/x86_64), verifying each one's sha256, and falls back to building
# both from source with cargo when there is no mantra binary for the platform or its download or
# checksum fails. A release without a bija asset (or one that fails its checksum) still installs
# mantra. Never uses sudo; installs to a per-user directory and adds it to PATH if needed.
# Full diagnostics remain available through `mantra doctor`.
#
# Environment overrides:
#   MANTRA_INSTALL_DIR   where to install (default: $HOME/.local/bin)
#   MANTRA_VERSION       release tag to install, e.g. v0.2.0 (default: latest)
#   MANTRA_FROM_SOURCE   set to 1 to skip release assets and always build from source
#   MANTRA_INSTALL_VERBOSE set to 1 for download details and the doctor report
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
STAGE_DIR=""

log() {
    if [ "${MANTRA_INSTALL_VERBOSE:-0}" = "1" ]; then printf '%s\n' "$*"; fi
}

err() {
    printf 'install.sh: error: %s\n' "$*" >&2
}

cleanup() {
    if [ -n "$STAGE_DIR" ]; then rm -rf "$STAGE_DIR"; fi
    for d in $TMP_DIRS; do
        rm -rf "$d"
    done
}
trap cleanup EXIT INT TERM

if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    printf '\n\033[1;31m  mantra\033[0m  ·  installing %s\n' "$VERSION"
else
    printf '\n  mantra  ·  installing %s\n' "$VERSION"
fi

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

# verify <file> <sha256-file>  — 0 when the checksum matches, 1 when it does not, 2 when no
# sha256 tool exists to check it with. Sets `expected` and `actual` for the caller's message.
verify() {
    expected=$(awk '{print $1}' "$2")
    if command -v sha256sum >/dev/null 2>&1; then
        actual=$(sha256sum "$1" | awk '{print $1}')
    elif command -v shasum >/dev/null 2>&1; then
        actual=$(shasum -a 256 "$1" | awk '{print $1}')
    else
        actual=""
        return 2
    fi
    [ -n "$actual" ] && [ "$expected" = "$actual" ]
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
bija_path=""

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
        status=0
        verify "$dl_dir/$asset" "$dl_dir/$asset.sha256" || status=$?
        if [ "$status" -eq 0 ]; then
            log "Checksum OK."
            chmod +x "$dl_dir/$asset"
            binary_path="$dl_dir/$asset"
        elif [ "$status" -eq 2 ]; then
            err "no sha256sum or shasum found; cannot verify the download. Falling back to building from source."
        else
            err "checksum mismatch for $asset (expected $expected, got ${actual:-none}); falling back to building from source."
        fi
    else
        log "No release asset available (or download failed); falling back to building from source."
    fi

    # bija rides along with mantra and every default model runs on it; a missing or bad bija
    # asset is still a note, never a reason to build everything from source (mantra says what to
    # do when it starts on a bija model without it, and a source build brings both).
    if [ -n "$binary_path" ]; then
        bija_asset="bija-${asset#mantra-}"
        if fetch "$base_url/$bija_asset" "$dl_dir/$bija_asset" 2>/dev/null && fetch "$base_url/$bija_asset.sha256" "$dl_dir/$bija_asset.sha256" 2>/dev/null; then
            status=0
            verify "$dl_dir/$bija_asset" "$dl_dir/$bija_asset.sha256" || status=$?
            if [ "$status" -eq 0 ]; then
                log "Downloaded $bija_asset; checksum OK."
                chmod +x "$dl_dir/$bija_asset"
                bija_path="$dl_dir/$bija_asset"
            else
                err "could not verify $bija_asset (expected $expected, got ${actual:-none}); not installing it."
            fi
        else
            log "No bija asset for this release; building from source gets it (MANTRA_FROM_SOURCE=1)."
        fi
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

    log "Building mantra and bija (cargo build --release)... this can take a few minutes."
    # --target-dir pins the output whatever CARGO_TARGET_DIR the caller has set.
    (cd "$src_dir/mantra/mantra_src" && cargo build --release --target-dir target)
    binary_path="$src_dir/mantra/mantra_src/target/release/mantra"
    # Releases before bija have no second binary; the -f check keeps them installable.
    if [ -f "$src_dir/mantra/mantra_src/target/release/bija" ]; then
        bija_path="$src_dir/mantra/mantra_src/target/release/bija"
    fi
fi

if [ ! -f "$binary_path" ]; then
    err "no mantra binary was produced; aborting."
    exit 1
fi

# --- install ----------------------------------------------------------------

mkdir -p "$INSTALL_DIR"
if [ -d "$INSTALL_DIR/mantra" ] || [ -d "$INSTALL_DIR/bija" ]; then
    err "installation target contains a directory named mantra or bija; existing files kept."
    exit 1
fi
if [ "${MANTRA_UPDATE:-0}" = "1" ] && [ -z "$bija_path" ]; then
    err "this release did not provide a verified bija binary; existing installation kept. Retry with MANTRA_FROM_SOURCE=1."
    exit 1
fi
# Stage on the same filesystem and rename, so a running mantra can update itself without
# ETXTBSY or a partly copied executable. Both downloads are verified before either is installed.
STAGE_DIR=$(mktemp -d "$INSTALL_DIR/.mantra-install.XXXXXX")
cp "$binary_path" "$STAGE_DIR/mantra"
chmod +x "$STAGE_DIR/mantra"
if [ -n "$bija_path" ]; then
    cp "$bija_path" "$STAGE_DIR/bija"
    chmod +x "$STAGE_DIR/bija"
    if [ -f "$INSTALL_DIR/bija" ]; then cp -p "$INSTALL_DIR/bija" "$STAGE_DIR/bija.previous"; fi
    mv -f "$STAGE_DIR/bija" "$INSTALL_DIR/bija"
fi
if ! mv -f "$STAGE_DIR/mantra" "$INSTALL_DIR/mantra"; then
    if [ -n "$bija_path" ]; then
        if [ -f "$STAGE_DIR/bija.previous" ]; then
            mv -f "$STAGE_DIR/bija.previous" "$INSTALL_DIR/bija"
        else
            rm -f "$INSTALL_DIR/bija"
        fi
    fi
    err "could not replace mantra; previous installation restored."
    exit 1
fi
log "Installed mantra to $INSTALL_DIR/mantra"
if [ -n "$bija_path" ]; then
    log "Installed bija to $INSTALL_DIR/bija"
elif [ -f "$INSTALL_DIR/bija" ]; then
    log "Kept the existing $INSTALL_DIR/bija; it may be older than this mantra."
fi

# --- PATH ---------------------------------------------------------------

case ":$PATH:" in
    *":$INSTALL_DIR:"*) on_path=1 ;;
    *) on_path=0 ;;
esac

if [ "$on_path" -eq 0 ] && [ "${MANTRA_UPDATE:-0}" != "1" ]; then
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

# --- compact completion -------------------------------------------------

installed_version=$("$INSTALL_DIR/mantra" --version 2>/dev/null || printf 'mantra')
printf '  %s ready  ·  %s\n' "$installed_version" "$INSTALL_DIR"
if [ "${MANTRA_UPDATE:-0}" = "1" ]; then
    printf '  Restart mantra to use the new version.\n\n'
else
    printf '  Start: mantra  ·  Add a model: /models\n'
    if [ "$on_path" -eq 0 ]; then printf '  Open a new terminal to use mantra.\n'; fi
    printf '  Update: mantra update  ·  Check: mantra doctor\n\n'
fi
if [ "${MANTRA_INSTALL_VERBOSE:-0}" = "1" ]; then
    "$INSTALL_DIR/mantra" doctor || log "Installation succeeded; doctor reported issues above."
fi

exit 0
