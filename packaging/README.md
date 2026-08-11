# Packaging & deployment

Three ways to get SoundNet onto a Linux machine, in order of how much you have
to type.

| | Use when | Command |
|---|---|---|
| **A. `.deb`** | You already built a package for this architecture | `sudo apt install ./soundnet-engine_*.deb` |
| **B. `install.sh`** | Fresh machine, nothing set up yet | `packaging/install.sh` |
| **C. Manual** | You want to see every step | See [README.md](../README.md) |

## Setting up a brand-new machine

```bash
sudo apt update && sudo apt install -y git
git clone -b claude/low-latency-network-audio-54k08x \
    git@github.com:hyousaku/soundnet.git ~/soundnet
cd ~/soundnet && packaging/install.sh --yes
```

Swap the clone URL for `https://github.com/hyousaku/soundnet.git` if the
machine has no SSH key registered. Then log out and back in — `install.sh`
adds you to the `audio` group, and the real-time limits only apply to a fresh
session.

Verify with `soundnet-engine --version`: it prints the git commit it was
built from, and `install.sh` has already checked that against the binary it
just built, so a mismatch would have stopped the install.

## A. Install a prebuilt `.deb`

```bash
sudo apt install ./soundnet-engine_0.1.0-1_arm64.deb
sudo usermod -aG audio $USER          # once per machine; log out and back in
sudo systemctl enable --now soundnet-engine@$USER.service
```

The package installs:

```
/usr/bin/soundnet-engine
/usr/lib/systemd/system/soundnet-engine@.service
/etc/security/limits.d/99-soundnet-realtime.conf   (conffile)
/usr/share/doc/soundnet-engine/
```

`apt remove soundnet-engine` stops any running instance and takes it all back
out (`apt purge` also drops the limits file).

Upgrading is just installing the newer `.deb` over the old one — the postinst
restarts whatever instances were already running, so the new binary takes over
without you touching `systemctl`.

## Building the `.deb`

```bash
packaging/build-deb.sh            # → dist/soundnet-engine_<ver>-<rev>_<arch>.deb
packaging/build-deb.sh --skip-web # reuse the existing web/dist (no npm needed)
```

**Build it on the same CPU architecture as the targets.** The binary is native,
and cross-compiling would also mean cross-building libroc — so build the arm64
package on one Raspberry Pi and copy it to the rest, and the amd64 one on a PC.

Two things the script handles that are easy to get wrong by hand:

- **Build order.** `rust-embed` bakes `web/dist` into the binary at compile
  time, and on stable Rust it cannot tell cargo that those files changed. The
  script builds the web UI first and then touches `control/web.rs` to force the
  re-embed — without that you can ship a package whose UI is one build behind
  its engine, which looks exactly like "my change had no effect".
- **Dependency names.** It asks `dpkg` which package owns each shared library
  the binary actually links, rather than hardcoding names: ALSA is
  `libasound2t64` on Debian trixie and Ubuntu 24.04 but `libasound2` on older
  releases, and naming the wrong one makes the package uninstallable. If a
  library is not owned by any package (roc-toolkit built from source, say) it
  is left out of `Depends` and the script says so — that package will install
  anywhere, but the target host has to provide that library the same way.

Environment overrides: `DEB_REVISION` (default `1`), `DEB_MAINTAINER`.

## B. `install.sh` on a fresh machine

```bash
git clone https://github.com/hyousaku/soundnet.git
cd soundnet
packaging/install.sh                 # add --yes to skip the prompts
```

It installs the build dependencies, makes sure **libroc 0.4** is present —
building roc-toolkit from source when the distro only ships 0.3, which is what
Ubuntu 24.04 and Debian bookworm do — checks Node.js and Rust, then builds and
installs via `build-deb.sh` and enables `soundnet-engine@<you>.service`.

Options: `--yes`, `--no-service`, `--user NAME`.

It is safe to re-run: that is also the upgrade path (`git pull &&
packaging/install.sh --yes`).

## libroc 0.4 is not optional

The FFI in `crates/roc-sys` targets the **0.4 ABI**. 0.3 links fine and then
misbehaves at runtime, so `install.sh` checks `ROC_VERSION_MAJOR`/`MINOR` in
the installed header rather than trusting that `libroc-dev` is installed.

| Distro | libroc |
|---|---|
| Debian trixie / Raspberry Pi OS trixie | `libroc0.4` in apt — just works |
| Ubuntu 24.04, Debian bookworm | only 0.3 — `install.sh` offers to build 0.4 into `/usr/local` |

The source build uses `--build-3rdparty=openfec`, because Debian has no
`libopenfec` package and without it routes with `fec: true` fail to start.

## Real-time scheduling

`99-soundnet-realtime.conf` grants the `audio` group `rtprio 95` and unlimited
`memlock`; the systemd unit carries the matching `LimitRTPRIO`/`LimitMEMLOCK`.
The engine raises its ALSA and transport threads to `SCHED_FIFO` and locks its
memory — but treats being denied as a warning, never a failure, so an engine
started by a user outside the `audio` group still runs, just with more jitter.

## Upgrading a fleet

The engines speak to each other over the wire, so **upgrade every machine in
the same session** unless a release explicitly says the change is
wire-compatible. A version skew shows up as routes that never leave `retrying`.
