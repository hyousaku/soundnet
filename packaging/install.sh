#!/usr/bin/env bash
#
# One-shot installer for a fresh Debian/Ubuntu/Raspberry Pi OS machine.
#
#   git clone https://github.com/hyousaku/soundnet.git
#   cd soundnet && packaging/install.sh
#
# It installs the build and runtime dependencies (including building
# roc-toolkit 0.4 from source when the distro only ships 0.3), builds the web
# UI and the engine in the right order, packages and installs them, and
# enables the systemd service.
#
# Options:
#   --yes           don't ask before installing packages or building roc
#   --no-service    install the binary but don't enable/start systemd
#   --user NAME     run the service as NAME (default: the invoking user)
#
# If you have several machines of the same architecture, you only need to do
# this once: `packaging/build-deb.sh` leaves a .deb in dist/ that installs on
# the rest with `sudo apt install ./soundnet-engine_*.deb`.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

ASSUME_YES=0
WANT_SERVICE=1
SERVICE_USER="${SUDO_USER:-${USER:-$(id -un)}}"

while [[ $# -gt 0 ]]; do
    case "$1" in
        --yes|-y) ASSUME_YES=1 ;;
        --no-service) WANT_SERVICE=0 ;;
        --user) SERVICE_USER="${2:?--user needs a name}"; shift ;;
        -h|--help) awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 0 ;;
        *) echo "unknown argument: $1 (try --help)" >&2; exit 2 ;;
    esac
    shift
done

if [[ "$(id -u)" -eq 0 ]]; then
    SUDO=""
else
    SUDO="sudo"
    command -v sudo >/dev/null || { echo "error: need root or sudo" >&2; exit 1; }
fi

say()  { printf '\n\033[1;36m==> %s\033[0m\n' "$*"; }
warn() { printf '\033[1;33mwarning: %s\033[0m\n' "$*" >&2; }
die()  { printf '\033[1;31merror: %s\033[0m\n' "$*" >&2; exit 1; }

confirm() {
    [[ $ASSUME_YES -eq 1 ]] && return 0
    read -r -p "$1 [Y/n] " reply
    [[ -z "$reply" || "$reply" =~ ^[Yy] ]]
}

command -v apt-get >/dev/null || die "this installer only handles apt-based distros.
On others, install: a C toolchain, pkg-config, ALSA dev headers, libroc 0.4 dev,
Node.js 18+, and Rust stable — then run packaging/build-deb.sh (Debian) or the
manual steps in README.md."

APT_UPDATED=0
apt_install() {
    if [[ $APT_UPDATED -eq 0 ]]; then
        $SUDO apt-get update
        APT_UPDATED=1
    fi
    $SUDO apt-get install -y --no-install-recommends "$@"
}

# --- 1. base toolchain ----------------------------------------------------
say "installing build dependencies"
apt_install build-essential pkg-config curl git ca-certificates \
            libasound2-dev

# --- 2. libroc 0.4 --------------------------------------------------------
# The engine's FFI targets the 0.4 ABI. 0.3 (Ubuntu 24.04, Debian bookworm)
# is not compatible — it links, then misbehaves at runtime, so check the
# header rather than merely whether libroc-dev is installed.
roc_header_version() {
    local h
    for h in /usr/local/include/roc/version.h /usr/include/roc/version.h; do
        if [[ -f "$h" ]]; then
            local major minor
            major="$(sed -n 's/^#define ROC_VERSION_MAJOR *\([0-9]*\).*/\1/p' "$h" | head -1)"
            minor="$(sed -n 's/^#define ROC_VERSION_MINOR *\([0-9]*\).*/\1/p' "$h" | head -1)"
            if [[ -n "$major" && -n "$minor" ]]; then
                echo "${major}.${minor}"
                return
            fi
        fi
    done
    echo ""
}

build_roc_from_source() {
    local version="${ROC_VERSION:-v0.4.0}"
    say "building roc-toolkit ${version} from source (this takes ~10 min on a Pi)"
    apt_install scons ragel gengetopt libuv1-dev libunwind-dev libspeexdsp-dev \
                libsox-dev libsndfile1-dev libssl-dev libpulse-dev libtool intltool autoconf automake make cmake
    local src="${TMPDIR:-/tmp}/roc-toolkit-${version}"
    rm -rf "$src"
    git clone --depth 1 --branch "$version" \
        https://github.com/roc-streaming/roc-toolkit.git "$src"
    (
        cd "$src"
        # --build-3rdparty=openfec builds FEC support in; Debian has no
        # libopenfec package, and without it `fec: true` routes fail to start.
        scons -Q --prefix=/usr/local --build-3rdparty=openfec
        $SUDO scons -Q --prefix=/usr/local --build-3rdparty=openfec install
    )
    $SUDO ldconfig
}

ROC_VER="$(roc_header_version)"
if [[ "$ROC_VER" != "0.4" ]]; then
    say "looking for libroc 0.4 (found: ${ROC_VER:-none})"
    $SUDO apt-get install -y --no-install-recommends libroc-dev 2>/dev/null || true
    ROC_VER="$(roc_header_version)"
fi
if [[ "$ROC_VER" != "0.4" ]]; then
    warn "this distro does not package libroc 0.4 (found: ${ROC_VER:-none})."
    confirm "Build roc-toolkit 0.4 from source into /usr/local?" \
        || die "cannot continue without libroc 0.4"
    build_roc_from_source
    ROC_VER="$(roc_header_version)"
    [[ "$ROC_VER" == "0.4" ]] || die "roc-toolkit build finished but headers still report ${ROC_VER:-none}"
fi
echo "libroc ${ROC_VER} ok"

# --- 3. Node.js -----------------------------------------------------------
node_major() { command -v node >/dev/null && node -v | sed 's/^v\([0-9]*\).*/\1/' || echo 0; }

if [[ "$(node_major)" -lt 18 ]]; then
    say "installing Node.js"
    apt_install nodejs npm || true
fi
if [[ "$(node_major)" -lt 18 ]]; then
    die "Node.js 18+ is required to build the web UI (found: $(node -v 2>/dev/null || echo none)).
Install a newer one from https://deb.nodesource.com/ or via nvm, then re-run this script."
fi
echo "node $(node -v) ok"

# --- 4. Rust --------------------------------------------------------------
if ! command -v cargo >/dev/null; then
    say "installing Rust (rustup, stable)"
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path --profile minimal
    export PATH="$HOME/.cargo/bin:$PATH"
fi
command -v cargo >/dev/null || die "cargo still not on PATH — open a new shell and re-run"
echo "cargo $(cargo --version | awk '{print $2}') ok"

# --- 5. build & install ---------------------------------------------------
# Prefer the .deb: it records what it installed, so `apt remove soundnet-engine`
# is a clean uninstall, and the same file installs on every other machine of
# this architecture without rebuilding.
if command -v dpkg-deb >/dev/null; then
    say "building the Debian package"
    packaging/build-deb.sh
    DEB="$(ls -t dist/soundnet-engine_*_"$(dpkg --print-architecture)".deb | head -1)"
    say "installing $DEB"
    $SUDO apt-get install -y "./$DEB"
    BINARY=/usr/bin/soundnet-engine
else
    say "building from source (no dpkg on this system)"
    (cd web && npm install --no-audit --no-fund && npm run build)
    touch crates/soundnet-engine/src/control/web.rs   # force the SPA re-embed
    cargo build --release -p soundnet-engine
    $SUDO install -m 755 target/release/soundnet-engine /usr/local/bin/
    $SUDO install -m 644 packaging/soundnet-engine.service \
        /etc/systemd/system/soundnet-engine@.service
    $SUDO install -m 644 packaging/99-realtime.conf \
        /etc/security/limits.d/99-soundnet-realtime.conf
    $SUDO systemctl daemon-reload
    BINARY=/usr/local/bin/soundnet-engine
fi

# --- 6. service -----------------------------------------------------------
if [[ $WANT_SERVICE -eq 1 ]]; then
    say "enabling soundnet-engine@${SERVICE_USER}.service"

    if ! id -nG "$SERVICE_USER" | tr ' ' '\n' | grep -qx audio; then
        $SUDO usermod -aG audio "$SERVICE_USER"
        warn "$SERVICE_USER was added to the 'audio' group — log out and back in
         before real-time scheduling limits apply to interactive runs."
    fi

    # An engine started by hand still owns port 7788, and systemd's failure to
    # bind is easy to misread: the service restart-loops on "Address already in
    # use" while the old process keeps serving a perfectly working-looking UI.
    if pgrep -x soundnet-engine >/dev/null; then
        echo "stopping running engines first"
        $SUDO systemctl stop 'soundnet-engine@*.service' 2>/dev/null || true
        $SUDO pkill -x soundnet-engine || true
        sleep 1
    fi

    $SUDO systemctl reset-failed "soundnet-engine@${SERVICE_USER}.service" 2>/dev/null || true
    $SUDO systemctl enable --now "soundnet-engine@${SERVICE_USER}.service"
    sleep 1
    $SUDO systemctl --no-pager --lines=10 status "soundnet-engine@${SERVICE_USER}.service" || true
fi

say "done"
echo "binary:  $BINARY ($("$BINARY" --version 2>/dev/null || echo 'version unknown'))"
echo "web UI:  http://$(hostname).local:7788/   (or http://<this host's IP>:7788/)"
if [[ $WANT_SERVICE -eq 0 ]]; then
    echo
    echo "Service not enabled (--no-service). To start it later:"
    echo "    sudo systemctl enable --now soundnet-engine@${SERVICE_USER}.service"
fi
