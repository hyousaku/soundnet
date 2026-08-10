#!/usr/bin/env bash
#
# Build a Debian package for soundnet-engine.
#
#   packaging/build-deb.sh              build the web UI, the engine, and package both
#   packaging/build-deb.sh --skip-web   reuse the existing web/dist (no npm needed)
#
# Output: dist/soundnet-engine_<version>-<rev>_<arch>.deb
#
# Run this on a machine with the SAME architecture as the targets — the binary
# is native, so an arm64 package has to be built on an arm64 box. One Pi can
# build the package for every other Pi; a PC builds the amd64 one. (Cross
# compiling would also need a cross-built libroc, which is more work than
# borrowing a Pi for five minutes.)
#
# Install the result on any machine of that architecture with:
#
#   sudo apt install ./soundnet-engine_<version>-<rev>_<arch>.deb
#
# Environment overrides: DEB_REVISION (default 1), DEB_MAINTAINER.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

SKIP_WEB=0
for arg in "$@"; do
    case "$arg" in
        --skip-web) SKIP_WEB=1 ;;
        -h|--help) awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0"; exit 0 ;;
        *) echo "unknown argument: $arg (try --help)" >&2; exit 2 ;;
    esac
done

need() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "error: $1 is required but not installed" >&2
        exit 1
    }
}
need cargo
need dpkg
need dpkg-deb

VERSION="$(sed -n '/^\[workspace\.package\]/,/^\[[a-z]/p' Cargo.toml |
           sed -n 's/^version *= *"\([^"]*\)".*/\1/p' | head -1)"
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+ ]]; then
    echo "error: could not read workspace.package.version from Cargo.toml" >&2
    exit 1
fi
REVISION="${DEB_REVISION:-1}"
ARCH="$(dpkg --print-architecture)"
MAINTAINER="${DEB_MAINTAINER:-SoundNet packaging <root@localhost>}"
PKGNAME="soundnet-engine_${VERSION}-${REVISION}_${ARCH}"

echo "==> building ${PKGNAME}.deb"

# --- 1. web UI ------------------------------------------------------------
# Order matters: rust-embed bakes web/dist into the binary at compile time, so
# the SPA has to exist (and be current) before cargo runs.
if [[ $SKIP_WEB -eq 0 ]]; then
    need npm
    echo "==> building web UI"
    if [[ -f web/package-lock.json ]]; then
        (cd web && npm ci --no-audit --no-fund) || (cd web && npm install --no-audit --no-fund)
    else
        (cd web && npm install --no-audit --no-fund)
    fi
    (cd web && npm run build)
elif [[ ! -d web/dist ]]; then
    echo "error: --skip-web given but web/dist/ does not exist" >&2
    exit 1
fi

# --- 2. engine ------------------------------------------------------------
# On stable Rust, rust-embed cannot tell cargo that web/dist changed, so a
# rebuilt SPA alone does not invalidate the crate. Touching the module that
# holds the #[derive(RustEmbed)] forces the re-embed — without this you can
# ship a package whose UI is one build behind its engine.
touch crates/soundnet-engine/src/control/web.rs
echo "==> building engine"
cargo build --release -p soundnet-engine

BIN="target/release/soundnet-engine"
[[ -x "$BIN" ]] || { echo "error: $BIN missing after build" >&2; exit 1; }

# --- 3. runtime dependencies ---------------------------------------------
# Resolve the packages that actually own the shared libraries this binary
# links against, rather than hardcoding names: Debian trixie and Ubuntu 24.04
# ship ALSA as libasound2t64 (the 64-bit time_t rename), older releases as
# libasound2, and a Depends on the wrong one makes the package uninstallable.
#
# A library built from source (roc-toolkit 0.4 on Ubuntu 24.04, say) belongs to
# no package at all. Naming one anyway would produce a .deb that apt refuses to
# install on the very machines it was built for, so in that case the dependency
# is left out and the operator is told what the resulting package assumes.
pkg_for_soname() {
    local soname="$1" fallback="$2" path pkg
    path="$(ldconfig -p | awk -v s="$soname" '$1 == s { print $NF; exit }')"
    if [[ -z "$path" ]]; then
        echo "warning: $soname is not in the linker cache; assuming package $fallback" >&2
        echo "$fallback"
        return
    fi
    pkg="$(dpkg -S "$(readlink -f "$path")" 2>/dev/null | head -1 | cut -d: -f1)" || true
    if [[ -z "$pkg" ]]; then
        echo "warning: $path is not owned by any package (built from source?)." >&2
        echo "         Leaving it out of Depends: this package will install anywhere," >&2
        echo "         but the target host must already provide $soname the same way." >&2
        return
    fi
    echo "$pkg"
}

DEPENDS="libc6"
for pkg in "$(pkg_for_soname libasound.so.2 libasound2)" "$(pkg_for_soname libroc.so.0.4 libroc0.4)"; do
    if [[ -n "$pkg" ]]; then
        DEPENDS="${DEPENDS}, ${pkg}"
    fi
done
echo "==> Depends: ${DEPENDS}"

# --- 4. staging tree ------------------------------------------------------
STAGE="target/deb/${PKGNAME}"
rm -rf "$STAGE"
install -d "$STAGE/DEBIAN" \
           "$STAGE/usr/bin" \
           "$STAGE/usr/lib/systemd/system" \
           "$STAGE/etc/security/limits.d" \
           "$STAGE/usr/share/doc/soundnet-engine"

install -m 755 "$BIN" "$STAGE/usr/bin/soundnet-engine"
strip "$STAGE/usr/bin/soundnet-engine" 2>/dev/null || true

# The repo's unit runs the binary from /usr/local/bin (where a from-source
# install puts it). A package must not touch /usr/local, so point this copy at
# /usr/bin — and fail loudly if the substitution silently stops matching.
sed 's|^ExecStart=/usr/local/bin/soundnet-engine|ExecStart=/usr/bin/soundnet-engine|' \
    packaging/soundnet-engine.service > "$STAGE/usr/lib/systemd/system/soundnet-engine@.service"
grep -q '^ExecStart=/usr/bin/soundnet-engine' "$STAGE/usr/lib/systemd/system/soundnet-engine@.service" || {
    echo "error: ExecStart rewrite failed — packaging/soundnet-engine.service changed shape" >&2
    exit 1
}

install -m 644 packaging/99-realtime.conf \
    "$STAGE/etc/security/limits.d/99-soundnet-realtime.conf"
install -m 644 README.md "$STAGE/usr/share/doc/soundnet-engine/README.md"

cat > "$STAGE/usr/share/doc/soundnet-engine/copyright" <<'EOF'
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: soundnet
Source: https://github.com/hyousaku/soundnet

Files: *
License: MIT
EOF

echo "/etc/security/limits.d/99-soundnet-realtime.conf" > "$STAGE/DEBIAN/conffiles"

INSTALLED_KB="$(du -ks --exclude=DEBIAN "$STAGE" | cut -f1)"

cat > "$STAGE/DEBIAN/control" <<EOF
Package: soundnet-engine
Version: ${VERSION}-${REVISION}
Architecture: ${ARCH}
Maintainer: ${MAINTAINER}
Section: sound
Priority: optional
Homepage: https://github.com/hyousaku/soundnet
Depends: ${DEPENDS}
Installed-Size: ${INSTALLED_KB}
Description: Low-latency LAN audio streaming engine with web UI
 SoundNet streams audio between Linux hosts over the LAN with a target
 end-to-end latency of around 10 ms, using roc-toolkit (RTP/UDP with
 clock-drift compensation, an adaptive jitter buffer and FEC) for transport.
 .
 Each host runs one small daemon that enumerates its ALSA capture and
 playback devices, announces itself over mDNS, and serves a patch-bay web UI
 on port 7788 — so a headless machine is configured from any browser on the
 same network.
 .
 Start it with: systemctl enable --now soundnet-engine@\$USER
EOF

cat > "$STAGE/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e

if [ "$1" = "configure" ]; then
    # /run/systemd/system exists only when systemd is actually the init
    # system — the standard guard, and it keeps chroots/containers quiet.
    if [ -d /run/systemd/system ]; then
        systemctl daemon-reload || true
        # Upgrades: restart whatever instances are already running so the new
        # binary actually takes over. `try-restart` is a no-op for units that
        # are not running, so a fresh install stays quiet.
        for unit in $(systemctl list-units --plain --no-legend 'soundnet-engine@*.service' 2>/dev/null | awk '{print $1}'); do
            systemctl try-restart "$unit" || true
        done
    fi

    # A unit in /etc always wins over the packaged one in /usr/lib, so a
    # leftover from-source install keeps the service pointed at
    # /usr/local/bin no matter how many times this package is upgraded. The
    # install looks like it worked and the old binary keeps running, which
    # is the single most confusing failure this project has had.
    if [ -e /etc/systemd/system/soundnet-engine@.service ]; then
        echo "soundnet-engine: WARNING: /etc/systemd/system/soundnet-engine@.service exists" >&2
        echo "  and OVERRIDES the unit this package just installed. The service will keep" >&2
        echo "  running the old binary until you remove it:" >&2
        echo "    sudo systemctl stop 'soundnet-engine@*'" >&2
        echo "    sudo rm /etc/systemd/system/soundnet-engine@.service" >&2
        echo "    sudo rm -f /usr/local/bin/soundnet-engine" >&2
        echo "    sudo systemctl daemon-reload && sudo systemctl start soundnet-engine@\$USER" >&2
    elif [ -e /usr/local/bin/soundnet-engine ]; then
        # Same class of problem, milder: /usr/local/bin comes first on $PATH,
        # so running the engine by hand gets the old build.
        echo "soundnet-engine: warning: /usr/local/bin/soundnet-engine still exists" >&2
        echo "  and shadows the packaged /usr/bin/soundnet-engine on \$PATH. Remove it with:" >&2
        echo "    sudo rm /usr/local/bin/soundnet-engine" >&2
    fi

    # $2 is the previously configured version — empty only on a first install,
    # which is the one time these instructions are news to anyone.
    if [ -z "$2" ]; then
        echo "soundnet-engine: enable it for a user that is in the 'audio' group, e.g."
        echo "    sudo usermod -aG audio \$USER   # log out and back in afterwards"
        echo "    sudo systemctl enable --now soundnet-engine@\$USER.service"
        echo "  then open http://$(hostname).local:7788/ from any browser on the LAN."
    fi
fi

exit 0
EOF

cat > "$STAGE/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e

if [ "$1" = "remove" ] && [ -d /run/systemd/system ]; then
    for unit in $(systemctl list-units --plain --no-legend 'soundnet-engine@*.service' 2>/dev/null | awk '{print $1}'); do
        systemctl stop "$unit" || true
    done
fi

exit 0
EOF

cat > "$STAGE/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e

if [ -d /run/systemd/system ]; then
    systemctl daemon-reload || true
fi

exit 0
EOF

chmod 755 "$STAGE/DEBIAN/postinst" "$STAGE/DEBIAN/prerm" "$STAGE/DEBIAN/postrm"

# --- 5. build -------------------------------------------------------------
mkdir -p dist
dpkg-deb --root-owner-group --build "$STAGE" "dist/${PKGNAME}.deb" >/dev/null

echo
echo "==> dist/${PKGNAME}.deb"
dpkg-deb --info "dist/${PKGNAME}.deb" | sed -n '2,8p'
echo
echo "Install it (on this machine or any other ${ARCH} host):"
echo "    sudo apt install ./dist/${PKGNAME}.deb"
