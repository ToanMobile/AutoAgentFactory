#!/bin/sh
# POSIX installer for AutoAgentFactory (`aaf` and `aaf-hook`).
#
# Usage:
#   sh scripts/install.sh [--version vX.Y.Z] [--dir ~/.local/bin] [--from-source]
#
# Environment overrides:
#   AAF_VERSION      Release tag (e.g. v0.1.0)
#   AAF_INSTALL_DIR  Destination directory (default: $HOME/.local/bin)
#   AAF_REPO         GitHub repository slug (default: ToanMobile/AutoAgentFactory)

set -eu

REPO="${AAF_REPO:-ToanMobile/AutoAgentFactory}"
VERSION="${AAF_VERSION:-}"
INSTALL_DIR="${AAF_INSTALL_DIR:-${HOME}/.local/bin}"
FROM_SOURCE=0

usage() {
    cat <<'EOF'
Install AutoAgentFactory (`aaf` and `aaf-hook`).

Usage:
  install.sh [options]

Options:
  --version <vX.Y.Z>   Install a specific release tag (default: latest release)
  --dir <path>         Destination directory (default: ~/.local/bin)
  --from-source        Build from local checkout (or GitHub source) via cargo
  -h, --help           Show this help message
EOF
}

die() {
    printf 'error: %s\n' "$*" >&2
    exit 1
}

need_cmd() {
    command -v "$1" >/dev/null 2>&1 || die "required command not found: $1"
}

download() {
    url="$1"
    dest="$2"
    if command -v curl >/dev/null 2>&1; then
        curl -fsSL --proto '=https' --tlsv1.2 -o "$dest" "$url"
    elif command -v wget >/dev/null 2>&1; then
        wget -q -O "$dest" "$url"
    else
        die "neither curl nor wget is available"
    fi
}

# Prints the latest release tag, or nothing when the repository has no
# published release yet (the caller then falls back to a source build).
resolve_latest_version() {
    api_url="https://api.github.com/repos/${REPO}/releases/latest"
    tmp_json="$1"
    if ! (download "$api_url" "$tmp_json") 2>/dev/null; then
        return 0
    fi
    sed -n 's/.*"tag_name"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$tmp_json" | head -n 1
}

validate_version() {
    case "$1" in
        v[0-9]*)
            ;;
        *)
            die "invalid version tag '$1' (expected v<semver>, e.g. v0.1.0)"
            ;;
    esac
    case "$1" in
        */*|*..*|*[[:space:]]*)
            die "invalid characters in version tag '$1'"
            ;;
    esac
}

detect_target() {
    os="$(uname -s)"
    arch="$(uname -m)"
    case "$os" in
        Darwin)
            case "$arch" in
                arm64|aarch64) printf 'aarch64-apple-darwin\n' ;;
                x86_64|amd64)  printf 'x86_64-apple-darwin\n' ;;
                *) die "unsupported macOS architecture: ${arch}" ;;
            esac
            ;;
        Linux)
            case "$arch" in
                x86_64|amd64)  printf 'x86_64-unknown-linux-gnu\n' ;;
                aarch64|arm64) printf 'aarch64-unknown-linux-gnu\n' ;;
                *) die "unsupported Linux architecture: ${arch}" ;;
            esac
            ;;
        *)
            die "unsupported operating system: ${os} (try --from-source)"
            ;;
    esac
}

compute_sha256() {
    file="$1"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$file" | awk '{print $1}'
    elif command -v shasum >/dev/null 2>&1; then
        shasum -a 256 "$file" | awk '{print $1}'
    else
        die "neither sha256sum nor shasum is available to verify SHA-256"
    fi
}

verify_sha256() {
    archive_path="$1"
    archive_name="$2"
    sums_path="$3"

    expected="$(awk -v name="$archive_name" '
        {
            f = $2
            sub(/^\*/, "", f)
            if (f == name) {
                print $1
                exit
            }
        }
    ' "$sums_path")"

    [ -n "$expected" ] || die "checksum entry for ${archive_name} not found in SHA256SUMS.txt"
    case "$expected" in
        *[!0-9a-fA-F]*)
            die "malformed SHA-256 entry for ${archive_name}"
            ;;
    esac
    if [ "${#expected}" -ne 64 ]; then
        die "invalid SHA-256 length for ${archive_name}"
    fi

    actual="$(compute_sha256 "$archive_path")"
    expected_lc="$(printf '%s' "$expected" | tr 'A-F' 'a-f')"
    actual_lc="$(printf '%s' "$actual" | tr 'A-F' 'a-f')"

    if [ "$actual_lc" != "$expected_lc" ]; then
        printf 'SHA-256 verification FAILED for %s\n' "$archive_name" >&2
        printf '  expected: %s\n' "$expected_lc" >&2
        printf '  actual:   %s\n' "$actual_lc" >&2
        exit 1
    fi
    printf 'Verified SHA-256 for %s (%s)\n' "$archive_name" "$actual_lc"
}

install_binary() {
    src="$1"
    name="$2"
    [ -f "$src" ] || die "binary missing in archive: ${name}"
    mkdir -p "$INSTALL_DIR"
    tmp_dst="${INSTALL_DIR}/.${name}.tmp.$$"
    cp "$src" "$tmp_dst"
    chmod 0755 "$tmp_dst"
    mv "$tmp_dst" "${INSTALL_DIR}/${name}"
}

install_from_source() {
    need_cmd cargo
    if [ -f "Cargo.toml" ] && [ -d "crates/aaf-cli" ] && [ -d "crates/aaf-hook" ]; then
        printf 'Building aaf and aaf-hook from local checkout...\n'
        cargo build --release --locked -p aaf-cli -p aaf-hook
        install_binary "target/release/aaf" "aaf"
        install_binary "target/release/aaf-hook" "aaf-hook"
    else
        need_cmd git
        src_dir="${TMP_DIR}/src"
        repo_url="https://github.com/${REPO}.git"
        printf 'Cloning %s...\n' "$repo_url"
        if [ -n "$VERSION" ]; then
            validate_version "$VERSION"
            git clone --depth 1 --branch "$VERSION" "$repo_url" "$src_dir"
        else
            git clone --depth 1 "$repo_url" "$src_dir"
        fi
        (
            cd "$src_dir"
            cargo build --release --locked -p aaf-cli -p aaf-hook
        )
        install_binary "${src_dir}/target/release/aaf" "aaf"
        install_binary "${src_dir}/target/release/aaf-hook" "aaf-hook"
    fi
}

while [ "$#" -gt 0 ]; do
    case "$1" in
        --version)
            [ "$#" -ge 2 ] || die "--version requires an argument"
            VERSION="$2"
            shift 2
            ;;
        --dir)
            [ "$#" -ge 2 ] || die "--dir requires an argument"
            INSTALL_DIR="$2"
            shift 2
            ;;
        --from-source)
            FROM_SOURCE=1
            shift
            ;;
        -h|--help)
            usage
            exit 0
            ;;
        *)
            die "unknown option: $1 (use --help)"
            ;;
    esac
done

TMP_DIR="$(mktemp -d "${TMPDIR:-/tmp}/aaf-install.XXXXXX")"
trap 'rm -rf "${TMP_DIR}"' EXIT INT TERM

if [ "$FROM_SOURCE" -eq 0 ] && [ -z "$VERSION" ]; then
    VERSION="$(resolve_latest_version "${TMP_DIR}/latest.json")"
    if [ -z "$VERSION" ]; then
        command -v cargo >/dev/null 2>&1 || die "no prebuilt release of ${REPO} is published yet and cargo is not installed; install Rust from https://rustup.rs and run this script again"
        printf 'No prebuilt release of %s yet: building from source with cargo.\n' "$REPO"
        FROM_SOURCE=1
    fi
fi

if [ "$FROM_SOURCE" -eq 1 ]; then
    install_from_source
else
    need_cmd tar
    TARGET="$(detect_target)"
    validate_version "$VERSION"

    ARCHIVE="aaf-${VERSION}-${TARGET}.tar.gz"
    BASE_URL="https://github.com/${REPO}/releases/download/${VERSION}"
    ARCHIVE_PATH="${TMP_DIR}/${ARCHIVE}"
    SUMS_PATH="${TMP_DIR}/SHA256SUMS.txt"

    printf 'Downloading %s (%s)...\n' "$ARCHIVE" "$VERSION"
    download "${BASE_URL}/${ARCHIVE}" "$ARCHIVE_PATH"
    download "${BASE_URL}/SHA256SUMS.txt" "$SUMS_PATH"

    verify_sha256 "$ARCHIVE_PATH" "$ARCHIVE" "$SUMS_PATH"

    EXTRACT_DIR="${TMP_DIR}/extract"
    mkdir -p "$EXTRACT_DIR"
    tar -xzf "$ARCHIVE_PATH" -C "$EXTRACT_DIR"

    if [ -f "${EXTRACT_DIR}/aaf-${VERSION}-${TARGET}/aaf" ]; then
        BIN_ROOT="${EXTRACT_DIR}/aaf-${VERSION}-${TARGET}"
    elif [ -f "${EXTRACT_DIR}/aaf" ]; then
        BIN_ROOT="${EXTRACT_DIR}"
    else
        die "could not locate 'aaf' binary inside ${ARCHIVE}"
    fi

    install_binary "${BIN_ROOT}/aaf" "aaf"
    install_binary "${BIN_ROOT}/aaf-hook" "aaf-hook"
fi

# On macOS, sign with a stable certificate + fixed identifier so TCC permissions
# survive upgrades (override via AAF_CODESIGN_IDENTITY or disable with "-").
if [ "$(uname -s)" = "Darwin" ] && [ "${AAF_CODESIGN_IDENTITY:-}" != "-" ] && command -v codesign >/dev/null 2>&1; then
    SIGN_ID="${AAF_CODESIGN_IDENTITY:-}"
    if [ -z "$SIGN_ID" ] && command -v security >/dev/null 2>&1; then
        SIGN_ID="$(security find-identity -v -p codesigning 2>/dev/null | awk '/Apple Development:/ {print $2; exit}' || true)"
    fi
    if [ -n "$SIGN_ID" ]; then
        if codesign --force --sign "$SIGN_ID" --identifier "com.autoagentfactory.aaf" "${INSTALL_DIR}/aaf" \
            && codesign --force --sign "$SIGN_ID" --identifier "com.autoagentfactory.aaf-hook" "${INSTALL_DIR}/aaf-hook" \
            && codesign --verify --strict "${INSTALL_DIR}/aaf" "${INSTALL_DIR}/aaf-hook" 2>/dev/null; then
            printf 'Signed aaf & aaf-hook with persistent macOS TCC identity.\n'
        fi
    fi
fi

printf 'Installed aaf and aaf-hook into %s\n' "$INSTALL_DIR"
"${INSTALL_DIR}/aaf" --version

case ":${PATH:-}:" in
    *":${INSTALL_DIR}:"*)
        ;;
    *)
        printf '\nNote: %s is not on your PATH. Add it to your shell profile:\n' "$INSTALL_DIR"
        printf '  export PATH="%s:$PATH"\n' "$INSTALL_DIR"
        ;;
esac

printf '\nNext, inside the git repository the factory should work on:\n'
printf '  aaf setup    # detects your agents, registers the hook, checks everything\n'
printf '  aaf ui       # then open http://127.0.0.1:4317/health from the link it prints\n'
