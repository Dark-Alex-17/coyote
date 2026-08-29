#!/usr/bin/env bash
set -euo pipefail

# coyote installer (Linux/macOS)
#
# Usage examples:
#   curl -fsSL https://raw.githubusercontent.com/Dark-Alex-17/coyote/main/scripts/install_coyote.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/Dark-Alex-17/coyote/main/scripts/install_coyote.sh | bash -s -- --version vX.Y.Z
#   BIN_DIR="$HOME/.local/bin" bash scripts/install_coyote.sh
#
# Flags / Env:
#   --version <tag>   Release tag (default: latest). Or set COYOTE_VERSION.
#   --bin-dir <dir>   Install directory (default: /usr/local/bin or ~/.local/bin). Or set BIN_DIR.

REPO="Dark-Alex-17/coyote"

usage() {
  echo "coyote installer (Linux/macOS)"
  echo
  echo "Options:"
  echo "  --version <tag>         Release tag (default: latest)"
  echo "  --bin-dir <dir>         Install directory (default: /usr/local/bin or ~/.local/bin)"
  echo "  -h, --help              Show help"
}

log() {
	echo "[coyote-install] $*"
}

need_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
  	echo "Error: required command '$1' not found" >&2
  	exit 1
  fi
}

http_get() {
  if [[ "$DL" == "curl" ]]; then
    curl -fsSL -H 'User-Agent: coyote-installer' "$1"
  else
    wget -qO- --header='User-Agent: coyote-installer' "$1"
  fi
}

smoke_test() {
  # The scratch dir may live on a noexec mount; if running in place fails,
  # retry from a probe file in the install directory before rejecting.
  local bin="$1"
  if "$bin" --version >/dev/null 2>&1; then return 0; fi
  local probe="${BIN_DIR}/.coyote-install-probe.$$"
  local ok=1
  if cp "$bin" "$probe" 2>/dev/null && chmod +x "$probe" 2>/dev/null; then
    if "$probe" --version >/dev/null 2>&1; then ok=0; fi
  fi
  rm -f "$probe"
  return "$ok"
}

main() {
  VERSION="${COYOTE_VERSION:-}"
  BIN_DIR="${BIN_DIR:-}"

  while [[ $# -gt 0 ]]; do
    case "$1" in
      --version) VERSION="$2"; shift 2;;
      --bin-dir) BIN_DIR="$2"; shift 2;;
      -h|--help) usage; exit 0;;
      *) echo "Unknown argument: $1" >&2; usage; exit 2;;
    esac
  done

  if [[ -n "$VERSION" && "$VERSION" =~ ^[0-9] ]]; then VERSION="v${VERSION}"; fi

  if [[ -z "${BIN_DIR}" ]]; then
    if [[ -w "/usr/local/bin" ]]; then
      BIN_DIR="/usr/local/bin"
    else
      BIN_DIR="${HOME}/.local/bin"
    fi
  fi
  mkdir -p "${BIN_DIR}"

  need_cmd uname
  need_cmd mktemp
  need_cmd tar

  if command -v curl >/dev/null 2>&1; then
    DL=curl
  elif command -v wget >/dev/null 2>&1; then
    DL=wget
  else
    echo "Error: need curl or wget" >&2
    exit 1
  fi

  UNAME_OS=$(uname -s | tr '[:upper:]' '[:lower:]')
  case "$UNAME_OS" in
    linux)  OS=linux ;;
    darwin) OS=darwin ;;
    *) echo "Error: unsupported OS '$UNAME_OS'" >&2; exit 1;;
  esac

  UNAME_ARCH=$(uname -m)
  case "$UNAME_ARCH" in
    x86_64|amd64) ARCH=x86_64 ;;
    aarch64|arm64) ARCH=aarch64 ;;
    *) echo "Error: unsupported arch '$UNAME_ARCH'" >&2; exit 1;;
  esac

  log "Target: ${OS}-${ARCH}"

  API_BASE="https://api.github.com/repos/${REPO}/releases"
  if [[ -z "${VERSION}" ]]; then
    RELEASE_URL="${API_BASE}/latest"
  else
    RELEASE_URL="${API_BASE}/tags/${VERSION}"
  fi

  WORKDIR="$(mktemp -d)"
  trap 'rm -rf "$WORKDIR"; rm -f "${BIN_DIR}/.coyote-install-probe.$$"' EXIT

  log "Fetching release metadata from $RELEASE_URL"
  JSON="$WORKDIR/release.json"
  if ! http_get "$RELEASE_URL" > "$JSON"; then
    echo "Error: failed to fetch release metadata. Check version tag." >&2
    exit 1
  fi

  ASSET_CANDIDATES=()
  if [[ "$OS" == "darwin" ]]; then
    if [[ "$ARCH" == "x86_64" ]]; then
      ASSET_CANDIDATES+=("coyote-x86_64-apple-darwin.tar.gz")
    else
      ASSET_CANDIDATES+=("coyote-aarch64-apple-darwin.tar.gz")
    fi
  elif [[ "$OS" == "linux" ]]; then
    LIBC="musl"
    if command -v getconf >/dev/null 2>&1 && getconf GNU_LIBC_VERSION >/dev/null 2>&1; then LIBC="gnu"; fi
    if ldd --version 2>&1 | grep -qi glibc; then LIBC="gnu"; fi

    if [[ "$LIBC" == "gnu" ]]; then
      # The gnu binary dynamically links OpenSSL 3. On Debian/Ubuntu, ldconfig lives
      # in /usr/sbin, which is often missing from non-root PATHs, so try its known
      # locations and fall back to probing the usual library directories directly.
      LIBSSL3=""
      for LDCONFIG in ldconfig /sbin/ldconfig /usr/sbin/ldconfig; do
        if command -v "$LDCONFIG" >/dev/null 2>&1; then
          if "$LDCONFIG" -p 2>/dev/null | grep -q 'libssl\.so\.3'; then LIBSSL3="yes"; fi
          break
        fi
      done
      if [[ -z "$LIBSSL3" ]]; then
        for LIBSSL_CANDIDATE in /usr/lib/*/libssl.so.3 /lib/*/libssl.so.3 /usr/lib64/libssl.so.3 /usr/lib/libssl.so.3 /usr/local/lib/libssl.so.3 /usr/local/lib/*/libssl.so.3; do
          if [[ -e "$LIBSSL_CANDIDATE" ]]; then LIBSSL3="yes"; break; fi
        done
      fi
      if [[ -n "$LIBSSL3" ]]; then
        ASSET_CANDIDATES+=("coyote-${ARCH}-unknown-linux-gnu.tar.gz")
      else
        log "glibc detected but OpenSSL 3 (libssl.so.3) not found; using musl build"
      fi
    fi

    ASSET_CANDIDATES+=("coyote-${ARCH}-unknown-linux-musl.tar.gz")
  else
    echo "Error: unsupported OS for this installer: $OS" >&2; exit 1
  fi

  DL_URLS=$(grep -oE '"browser_download_url":[[:space:]]*"[^"]+"' "$JSON" \
    | sed -E 's/.*"browser_download_url":[[:space:]]*"//; s/"$//' \
    || true)

  INSTALLED=""
  TRIED=()
  ATTEMPT=0
  for candidate in "${ASSET_CANDIDATES[@]}"; do
    ASSET_URL=""
    while IFS= read -r url; do
      [[ -z "$url" ]] && continue
      if [[ "$url" == */"$candidate" ]]; then
        ASSET_URL="$url"
        break
      fi
    done <<< "$DL_URLS"

    if [[ -z "$ASSET_URL" ]]; then
      TRIED+=("$candidate: no matching release asset")
      continue
    fi

    ATTEMPT=$((ATTEMPT + 1))
    WORK="$WORKDIR/attempt-$ATTEMPT"
    mkdir -p "$WORK"

    log "Selected asset: $candidate"
    log "Download URL: $ASSET_URL"

    ARCHIVE="$WORK/asset"
    if [[ "$DL" == "curl" ]]; then
      if ! curl -fL -H 'User-Agent: coyote-installer' "$ASSET_URL" -o "$ARCHIVE"; then
        log "Failed to download $candidate; trying next candidate"
        TRIED+=("$candidate: download failed")
        continue
      fi
    else
      if ! wget -q --header='User-Agent: coyote-installer' "$ASSET_URL" -O "$ARCHIVE"; then
        log "Failed to download $candidate; trying next candidate"
        TRIED+=("$candidate: download failed")
        continue
      fi
    fi

    EXTRACTED_DIR="$WORK/extracted"; mkdir -p "$EXTRACTED_DIR"

    if tar -tf "$ARCHIVE" >/dev/null 2>&1; then
      if ! tar -xzf "$ARCHIVE" -C "$EXTRACTED_DIR"; then
        log "Failed to extract $candidate; trying next candidate"
        TRIED+=("$candidate: extract failed")
        continue
      fi
    else
      if command -v unzip >/dev/null 2>&1; then
        if ! unzip -q "$ARCHIVE" -d "$EXTRACTED_DIR"; then
          log "Failed to extract $candidate; trying next candidate"
          TRIED+=("$candidate: extract failed")
          continue
        fi
      else
        log "Unknown archive format for $candidate and 'unzip' is not available; trying next candidate"
        TRIED+=("$candidate: unknown archive format and 'unzip' unavailable")
        continue
      fi
    fi

    BIN_PATH=""
    while IFS= read -r -d '' f; do
      base=$(basename "$f")
      if [[ "$base" == "coyote" ]]; then
      	BIN_PATH="$f"
      	break
      fi
    done < <(find "$EXTRACTED_DIR" -type f -print0)

    if [[ -z "$BIN_PATH" ]]; then
      log "Could not find 'coyote' binary in $candidate; trying next candidate"
      TRIED+=("$candidate: no 'coyote' binary in archive")
      continue
    fi

    chmod +x "$BIN_PATH"
    if ! smoke_test "$BIN_PATH"; then
      log "Downloaded $candidate but it failed to run on this system; trying next candidate"
      TRIED+=("$candidate: binary failed to run on this system")
      continue
    fi

    install -m 0755 "$BIN_PATH" "${BIN_DIR}/coyote"
    INSTALLED="$candidate"
    break
  done

  if [[ -z "$INSTALLED" ]]; then
    echo "Error: no usable asset found for ${OS}-${ARCH}. Tried:" >&2
    for t in "${TRIED[@]}"; do echo "  - $t" >&2; done
    exit 1
  fi

  log "Installed: ${BIN_DIR}/coyote"

  case ":$PATH:" in
    *":${BIN_DIR}:"*) ;;
    *)
      log "Note: ${BIN_DIR} is not in PATH. Add it, e.g.:"
      log "  export PATH=\"${BIN_DIR}:\$PATH\""
      ;;
  esac

  log "Done. Try: coyote --help"
}

main "$@"
