#!/usr/bin/env bash
# treehouse installer — curl -fsSL "https://raw.githubusercontent.com/quangdang46/treehouse_rust/main/install.sh" | bash
#
# Installs the treehouse binary for parallel AI coding agent worktrees.
set -euo pipefail
umask 022

# === Configuration ===
BINARY_NAME="treehouse"
OWNER="quangdang46"
REPO="treehouse_rust"
DEST="${DEST:-$HOME/.local/bin}"
VERSION="${VERSION:-}"
QUIET=0
EASY=0
VERIFY=0
FROM_SOURCE=0
UNINSTALL=0
MAX_RETRIES=3
DOWNLOAD_TIMEOUT=120
LOCK_DIR="/tmp/${BINARY_NAME}-install.lock.d"
TMP=""

# === Logging ===
log_info()    { [ "$QUIET" -eq 1 ] && return; echo "[${BINARY_NAME}] $*" >&2; }
log_warn()    { echo "[${BINARY_NAME}] WARN: $*" >&2; }
log_success() { [ "$QUIET" -eq 1 ] && return; echo "✓ $*" >&2; }
die()         { echo "ERROR: $*" >&2; exit 1; }

# === Cleanup & lock ===
cleanup() { rm -rf "$TMP" "$LOCK_DIR" 2>/dev/null || true; }
trap cleanup EXIT
acquire_lock() {
    mkdir "$LOCK_DIR" 2>/dev/null || die "Another install is running. If stuck, run: rm -rf $LOCK_DIR"
    echo $$ > "$LOCK_DIR/pid"
}

# === Argument parsing (supports --flag value and --flag=value) ===
while [ $# -gt 0 ]; do
    case "$1" in
        --dest)       DEST="$2";  shift 2 ;;
        --dest=*)     DEST="${1#*=}"; shift ;;
        --version)    VERSION="$2"; shift 2 ;;
        --version=*)  VERSION="${1#*=}"; shift ;;
        --system)     DEST="/usr/local/bin"; shift ;;
        --easy-mode)  EASY=1; shift ;;
        --verify)     VERIFY=1; shift ;;
        --from-source) FROM_SOURCE=1; shift ;;
        --quiet|-q)   QUIET=1; shift ;;
        --uninstall)  UNINSTALL=1; shift ;;
        -h|--help)
            sed -n '2,/^$/p' "$0" | sed 's/^# \?//'
            exit 0 ;;
        *) shift ;;
    esac
done

# === Uninstall ===
if [ "$UNINSTALL" -eq 1 ]; then
    rm -f "$DEST/$BINARY_NAME"
    for rc in "$HOME/.bashrc" "$HOME/.zshrc"; do
        [ -f "$rc" ] && sed -i "/${BINARY_NAME} installer/d" "$rc" 2>/dev/null || true
    done
    echo "✓ ${BINARY_NAME} uninstalled from $DEST"
    exit 0
fi

# === Platform detection ===
detect_platform() {
    local os arch
    case "$(uname -s)" in
        Linux*)  os="linux" ;;
        Darwin*) os="darwin" ;;
        MINGW*|MSYS*|CYGWIN*) os="windows" ;;
        *) die "Unsupported OS: $(uname -s)" ;;
    esac
    case "$(uname -m)" in
        x86_64|amd64)  arch="x86_64" ;;
        aarch64|arm64) arch="aarch64" ;;
        *) die "Unsupported architecture: $(uname -m)" ;;
    esac
    echo "${os}_${arch}"
}

# === Version resolution (GitHub API, fallback to redirect) ===
resolve_version() {
    [ -n "$VERSION" ] && return 0
    log_info "Resolving latest version..."
    VERSION=$(curl -fsSL --connect-timeout 10 --max-time 30 \
        "https://api.github.com/repos/${OWNER}/${REPO}/releases/latest" 2>/dev/null \
        | grep '"tag_name":' | sed -E 's/.*"([^"]+)".*/\1/') || true
    if ! [[ "$VERSION" =~ ^v[0-9] ]]; then
        VERSION=$(curl -fsSL -o /dev/null -w '%{url_effective}' \
            "https://github.com/${OWNER}/${REPO}/releases/latest" 2>/dev/null \
            | sed -E 's|.*/tag/||') || true
    fi
    [[ "$VERSION" =~ ^v[0-9] ]] || die "Could not resolve the latest version. Check releases: https://github.com/${OWNER}/${REPO}/releases"
    log_info "Latest version: $VERSION"
}

# === Download with retry + resume ===
download_file() {
    local url="$1" dest="$2" partial="${2}.part" attempt=0
    while [ $attempt -lt $MAX_RETRIES ]; do
        attempt=$((attempt + 1))
        log_info "Downloading $url → $dest (attempt $attempt/$MAX_RETRIES)"
        if curl -fL --connect-timeout 30 --max-time "$DOWNLOAD_TIMEOUT" \
              $( [ -s "$partial" ] && echo "--continue-at -" ) \
              -sS --retry 2 --retry-delay 3 \
              -o "$partial" "$url"; then
            mv -f "$partial" "$dest"
            return 0
        fi
        [ $attempt -lt $MAX_RETRIES ] && { log_warn "Retrying in 3s..."; sleep 3; }
    done
    return 1
}

# === Checksum verification (optional sha256 sidecar) ===
verify_checksum() {
    local archive="$1" checksum_url="$2"
    local checksum_file="${TMP}/checksum.sha256"
    if download_file "$checksum_url" "$checksum_file" 2>/dev/null; then
        local expected actual
        expected=$(awk '{print $1}' "$checksum_file")
        actual=$( (sha256sum "$archive" 2>/dev/null || shasum -a 256 "$archive" 2>/dev/null) | awk '{print $1}')
        if [ "$expected" = "$actual" ]; then
            log_info "Checksum verified"
        else
            die "Checksum mismatch! Expected: $expected, Got: $actual"
        fi
    else
        log_warn "No checksum sidecar found — skipping verification"
    fi
}

# === Atomic binary installation ===
install_binary_atomic() {
    local src="$1" dest="$2" tmp="${DEST}/$(basename "$2").tmp.$$"
    install -m 0755 "$src" "$tmp" && mv -f "$tmp" "$dest" || { rm -f "$tmp"; die "Failed to install binary to $dest"; }
}

# === PATH update (easy-mode) ===
maybe_add_path() {
    case ":${PATH}:" in *":$DEST:"*) return 0 ;; esac
    if [ "$EASY" -eq 1 ]; then
        for rc in "$HOME/.zshrc" "$HOME/.bashrc" "$HOME/.profile"; do
            if [ -f "$rc" ] && [ -w "$rc" ]; then
                grep -qF "$DEST" "$rc" 2>/dev/null && continue
                printf '\nexport PATH="%s:$PATH"  # %s installer\n' "$DEST" "$BINARY_NAME" >> "$rc"
            fi
        done
        log_warn "PATH updated — restart your shell or run: export PATH=\"$DEST:\$PATH\""
    else
        log_warn "Add to PATH: export PATH=\"$DEST:\$PATH\"  (or rerun with --easy-mode)"
    fi
}

# === Build from source fallback ===
build_from_source() {
    log_info "Building $BINARY_NAME from source..."
    command -v cargo >/dev/null 2>&1 || die "cargo not found. Install Rust: https://rustup.rs"
    command -v git   >/dev/null 2>&1 || die "git not found"
    git clone --depth 1 "https://github.com/${OWNER}/${REPO}.git" "$TMP/src"
    (cd "$TMP/src" && CARGO_TARGET_DIR="$TMP/target" cargo build --release --bin "$BINARY_NAME")
    install_binary_atomic "$TMP/target/release/$BINARY_NAME" "$DEST/$BINARY_NAME"
}

# === Main install ===
main() {
    acquire_lock
    TMP=$(mktemp -d)
    mkdir -p "$DEST"

    local platform; platform=$(detect_platform)
    log_info "Platform: $platform | Destination: $DEST"

    if [ "$FROM_SOURCE" -eq 1 ]; then
        build_from_source
    else
        resolve_version
        local arch="${platform##*_}" os="${platform%%_*}" ext="tar.gz"
        [ "$os" == "windows" ] && ext="zip"
        local archive="${BINARY_NAME}-${VERSION}-${os}-${arch}.${ext}"
        local url="https://github.com/${OWNER}/${REPO}/releases/download/${VERSION}/${archive}"

        if download_file "$url" "$TMP/$archive"; then
            verify_checksum "$TMP/$archive" "${url}.sha256"
            case "$archive" in
                *.tar.gz) tar -xzf "$TMP/$archive" -C "$TMP" ;;
                *.zip)    unzip -qo "$TMP/$archive" -d "$TMP" ;;
            esac
            # Locate the binary by name (it may be nested under a `tar` top dir).
            local bin; bin=$(find "$TMP" -name "$BINARY_NAME" -type f -perm -111 2>/dev/null | head -1)
            [ -n "$bin" ] || die "Binary '$BINARY_NAME' not found inside archive"
            install_binary_atomic "$bin" "$DEST/$BINARY_NAME"
        else
            log_warn "Binary download failed — falling back to source build..."
            build_from_source
        fi
    fi

    maybe_add_path

    if [ "$VERIFY" -eq 1 ]; then
        "$DEST/$BINARY_NAME" --version || die "Post-install verification failed"
    fi

    echo ""
    echo "✓ ${BINARY_NAME} installed → $DEST/$BINARY_NAME"
    echo "  $( "$DEST/$BINARY_NAME" --version 2>/dev/null || echo 'unknown version' )"
    echo ""
    echo "  Get started:  $BINARY_NAME --help"
}

# curl|bash safety: buffer entire script before executing so a truncated pipe
# never runs a half-parsed script.
if [[ "${BASH_SOURCE[0]:-}" == "${0:-}" ]] || [[ -z "${BASH_SOURCE[0]:-}" ]]; then
    { main "$@"; }
fi
