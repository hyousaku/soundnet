# SoundNet

Low-latency **LAN audio streaming** between Linux hosts (including
Raspberry Pi 4). One small Rust daemon per host, a shared browser UI you can
open from anywhere on the LAN, patch bay–style routing between USB / built-in
sound cards on different machines, and a target end-to-end latency of ~10 ms
**over wired Gigabit** — Wi-Fi works but clicks under load, and is for
verification rather than production (see [Network](#network-wired-only-in-practice)).

The transport is [roc-toolkit](https://github.com/roc-streaming/roc-toolkit) —
so you get clock-drift compensation, adaptive jitter buffer, and FEC out of the
box.

```
┌───────────── LAN ─────────────┐
│                                │
│  RPi4  ──►  soundnet-engine ──┼── UDP/RTP (+FEC) ──► soundnet-engine ──► USB DAC
│  (USB mic, headless)           │                     (desktop PC)
│                                │
│      ↑                         │
│      │ WebSocket + REST + SPA  │
│      │                         │
│  Laptop browser  ──────────────┘   ← opens http://raspi.local:7788/ and
│                                       patches everything visually.
└────────────────────────────────┘
```

## Features

- **Auto-discovery** of engines on the LAN via mDNS (`_soundnet._udp`)
- **Web UI** served by the engine itself (no separate install on the client)
  — open `http://<hostname>.local:7788/` in any browser
- **Patch bay** — drag between source and destination nodes to create a
  route, adjust parameters per route
- **Per-route tuning**: sample rate, sample format, channel count,
  ALSA `period_size`, roc `target_latency`, FEC on/off
- **Preview tones** — 440 Hz / 1 kHz virtual capture ports on every engine so
  you can verify the pipeline without external gear
- **USB audio interfaces + built-in audio** — anything ALSA can see appears
  as a port (including `default` for the system default)
- **Headless-friendly** — the engine has no display dependency; configure it
  from another machine's browser
- **Single-binary distribution** — the SPA is baked into `soundnet-engine`
  via `rust-embed`

## Install

Fresh Debian / Ubuntu / Raspberry Pi OS machine, one command:

```bash
git clone https://github.com/hyousaku/soundnet.git
cd soundnet
packaging/install.sh          # --yes to skip prompts, --no-service to skip systemd
```

It installs the dependencies (including building roc-toolkit 0.4 from source if
your distro only ships 0.3), builds the web UI and engine in the right order,
installs them as a `.deb`, and enables `soundnet-engine@<you>.service`.

Adding more machines of the same architecture? Don't repeat the build — copy
the package it left in `dist/`:

```bash
scp dist/soundnet-engine_*_arm64.deb pi@other-pi:
ssh pi@other-pi 'sudo apt install ./soundnet-engine_*_arm64.deb &&
                 sudo usermod -aG audio $USER &&
                 sudo systemctl enable --now soundnet-engine@$USER.service'
```

See [packaging/README.md](packaging/README.md) for the details — building the
`.deb` yourself, what it installs, and the libroc 0.4 requirement. The rest of
this section describes the same thing done by hand.

## Runtime dependencies

- Linux kernel with ALSA (any modern distro)
- **`libroc` (roc-toolkit) 0.4.x** — Debian trixie / Raspberry Pi OS trixie:
  `libroc0.4`. Older `libroc0.3` (Ubuntu 24.04) is **not** ABI-compatible.
- `libasound2` — usually already installed
- (Recommended) `PREEMPT_RT` kernel and membership in the `audio` group for
  real-time scheduling

> **FEC availability:** Debian trixie's `libroc0.4` is built **with** FEC
> (pulls in `libopenfec`), so `fec: true` routes work out of the box. On
> Ubuntu 24.04 the older `libroc0.3` package is built **without** FEC and
> is also the wrong ABI for this project — either upgrade to trixie or
> build roc-toolkit 0.4 from source with `--with-openfec`.

## Build dependencies

- Rust stable (`rustup default stable`)
- Node.js 20+ and npm (for the web UI)
- `libasound2-dev`, `libroc-dev`, `pkg-config`
- `libclang-dev` — `crates/roc-sys` generates its FFI from roc's headers with
  bindgen, which parses them with libclang. Build-time only; it is not a
  dependency of the resulting binary or of the `.deb`.

Debian/Ubuntu/Raspberry Pi OS:

```bash
sudo apt install libasound2-dev libroc-dev pkg-config build-essential libclang-dev
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# Node via nvm or your distro's package
```

## Build & run by hand

```bash
git clone https://github.com/hyousaku/soundnet.git
cd soundnet

# 1) Build the web UI (produces web/dist/, embedded into the binary).
cd web && npm install && npm run build && cd ..

# 2) Build the engine.
cargo build --release -p soundnet-engine

# 3) Run on each host you want to participate.
./target/release/soundnet-engine
```

Then open `http://<hostname>.local:7788/` in any browser on the same LAN and
drag between nodes to route audio.

## CLI flags

```
soundnet-engine [--bind 0.0.0.0:7788] [--name my-node] [--audio-port 10001]
```

- `--bind` — TCP address for the HTTP/WebSocket control plane. Default
  `0.0.0.0:7788`. Set to a specific interface IP to keep the UI off other
  networks.
- `--name` — advertised hostname (defaults to system hostname).
- `--audio-port` — UDP port for roc audio (source). Repair uses `+1` when FEC is on.

## Running as a systemd service

If you have been running the engine by hand (`./target/release/soundnet-engine`,
possibly under `nohup`), **stop it first**. systemd cannot bind port 7788 while
another copy holds it, and the failure is easy to misread: the service enters a
restart loop logging `Address already in use (os error 98)` while the old
process keeps serving the UI perfectly well — so the web page looks fine and it
appears as though a freshly built binary simply had no effect.

```bash
pkill -f soundnet-engine
sleep 1
pgrep -f soundnet-engine || echo "nothing left running"

sudo install -m 755 target/release/soundnet-engine /usr/local/bin/
sudo cp packaging/soundnet-engine.service /etc/systemd/system/soundnet-engine@.service
sudo cp packaging/99-realtime.conf /etc/security/limits.d/99-soundnet-realtime.conf
sudo systemctl daemon-reload
sudo usermod -aG audio $USER          # log out and back in after this
sudo systemctl enable --now soundnet-engine@$USER.service
systemctl status soundnet-engine@$USER.service --no-pager
```

### Deploying a new build

The web UI is compiled into the binary by `rust-embed`, so **build `web/` before
building the engine** or the embedded UI will be the previous one. On stable
Rust, rust-embed also cannot tell cargo that `web/dist` changed, so the engine
crate has to be invalidated by hand:

```bash
git pull
cd web && npm run build && cd ..
touch crates/soundnet-engine/src/control/web.rs   # force the SPA re-embed
cargo build --release -p soundnet-engine
sudo install -m 755 target/release/soundnet-engine /usr/local/bin/
sudo systemctl restart soundnet-engine@$USER.service
```

`packaging/install.sh --yes` (or `packaging/build-deb.sh` + `apt install`) does
all of that for you, in the right order.

If a service has been failing repeatedly, systemd may refuse to start it until
you clear the failure counter with
`sudo systemctl reset-failed soundnet-engine@$USER.service`.

## Security: this trusts your LAN

**Anything that can reach port 7788 has full control of the engine.** There is
no authentication, and none is planned — the design assumes a network you
already trust with an open microphone.

Concretely, a peer on the same network can list the machine's sound devices,
create and delete routes, re-patch audio, and stream a capture port anywhere
the engine can reach. On a home or studio LAN that is the same trust you
already extend by advertising over mDNS. On a conference Wi-Fi, a shared
office network, or anything with guests on it, it is not.

So:

- **Do not port-forward 7788**, and do not put the engine on a public IP.
- On a network you don't control, bind it to one interface —
  `--bind 192.168.1.20:7788` — or to loopback and reach it over an SSH tunnel:
  `ssh -L 7788:localhost:7788 host`.
- If you want it reachable more widely, put a reverse proxy with real
  authentication in front. The engine will not grow a login of its own.

The UI is served from the engine's own origin and needs no CORS, so the
control plane sends no cross-origin headers at all. This matters more than it
sounds: with a permissive policy, any web page anyone on that machine happened
to visit could script this API from their browser, which would extend the
trust boundary from "our LAN" to "every site we browse". `SOUNDNET_DEV_ORIGIN`
re-enables CORS for one named origin, for the Vite dev server, and warns
loudly when it does.

## Network: wired only, in practice

**Use a wire.** Measured on the two deployed machines, a wired link produces
no audible artefacts at all; the same route over Wi-Fi clicks audibly on any
sustained note. Wi-Fi is supported and useful — for checking that a machine
works, for a temporary setup, for monitoring — but it is not the transport
this project is for.

The reason is not fixable from here. Wi-Fi is CSMA/CA: a station that wants to
transmit must wait until the air is free, and retransmit when a frame is lost.
The resulting delay varies by milliseconds to tens of milliseconds and cannot
be predicted, while the whole design rests on a jitter buffer sized in single
milliseconds. A packet that misses its slot is a gap in the waveform, and a
gap in a waveform is a click. This is the same reason Dante is a wired
technology and AES67 assumes a managed network.

If you do run over Wi-Fi, three things make it tolerable:

| | |
|---|---|
| `target_latency_ms` **40–80** | The jitter buffer is the only thing absorbing that variance. This is the setting that matters. |
| `frames_per_period` **256** | Cuts the packet rate to a quarter. Wi-Fi pays a fixed airtime cost per frame regardless of size, so many small packets is its worst case — and a longer FEC block also survives the consecutive losses Wi-Fi tends to produce. |
| `iw dev wlan0 set power_save off` | Client power saving casually adds tens of milliseconds. Persist it with a `wifi.powersave = 2` drop-in under `/etc/NetworkManager/conf.d/`. |

Prefer 5 GHz. Expect ~50 ms end to end, and treat anything under 20 ms as
wired-only territory.

The engine has no opinion about which interface you use — pin one per node
under **This node → Egress interface** in the UI — so the same node can be
wired for a session and wireless for a soundcheck.

## Latency tuning

Start with the defaults (48 kHz / S24 / 2 ch / period 128 / target 10 ms /
FEC on) — that's tuned for a USB class-compliant interface on RPi4 with a
target end-to-end latency of ~12–18 ms.

Push it lower:

- Drop `frames_per_period` to 64 or 32 (may xrun on RPi under load)
- Drop `target_latency_ms` to 5 or 3
- Set FEC off if your LAN drops zero packets

Back off:

- Raise `target_latency_ms` to 40–80 if the network is jittery (Wi-Fi, VPN —
  see [Network](#network-wired-only-in-practice))
- Keep FEC on

All parameters are editable per-route in the UI while audio is running — the
worker restarts transparently.

### Measuring it for real

The `latency` column adds up what each engine can account for. It cannot see
the converters, and it is an accounting rather than a measurement. To get a
number that includes everything, put a cable in the loop and measure it:

```bash
sudo apt install python3-alsaaudio python3-numpy

# 1. The hardware floor. Cable from the interface's output to its own input.
tools/measure-latency.py --out plughw:1,0 --in plughw:1,0

# 2. The same floor plus one trip through SoundNet: route this machine's
#    input to the far end, and cable the far end's output back to this
#    machine's input.
tools/measure-latency.py --out plughw:1,0 --in plughw:2,0
```

Subtract the first from the second. The tool emits a chirp and
cross-correlates the return, which finds the signal ~45 dB below full scale —
so "nothing detected" means the loop is genuinely broken (a mute, an input
gain at zero, an interface routing switch), and it says so rather than
printing a number.

Both streams run from one lock-step loop so they share a time base. Two
separate processes — `aplay` and `arecord`, say — do not, and diffing their
timestamps measures the scheduler rather than the audio path.

## Development

```bash
# Run the engine on your dev machine. SOUNDNET_DEV_ORIGIN lets the Vite dev
# server talk to it cross-origin; without it the engine sends no CORS headers
# at all (see Security above), which is what you want in production and not
# what you want at :5173.
SOUNDNET_DEV_ORIGIN=http://localhost:5173 cargo run -p soundnet-engine

# Separately, run the Vite dev server (proxies /api and /ws to the engine)
cd web && npm run dev
# then open http://localhost:5173
```

Other environment variables:

- `RUST_LOG` — standard `tracing` filter, e.g. `info,soundnet_engine=debug`.
- `SOUNDNET_ROC_LOG` — libroc's own log level (`error` by default; `debug`
  shows jitter-buffer and latency-tuner activity, at a cost to the audio
  threads it is reporting on).

Tests:

```bash
cargo test --workspace
```

### Continuous integration

`.github/workflows/ci.yml` runs on pushes to `main` and on every pull
request. It builds the web UI (`tsc --noEmit && vite build`), hands the
result to a second job — the engine embeds `web/dist` at compile time, so it
does not build without it — and there runs `cargo fmt --all --check`,
`cargo clippy --workspace --all-targets -- -D warnings` and
`cargo test --workspace`.

No Ubuntu or Debian release packages libroc 0.4 (noble ships 0.3.0, which is
a different ABI), so CI builds it from source the same way
`packaging/install.sh` does and caches the result. The first run on a fresh
cache takes a few minutes longer.

What CI does **not** cover, and cannot:

- **Anything involving a sound card.** No audio device, no network peer, no
  xruns, no drift. Every interesting bug in this project so far has needed
  hardware and a pair of ears; CI catches the ones that are cheaper to catch.
- **arm64.** The Raspberry Pi build is verified by building on the Pi.
- **The `.deb`.** `packaging/build-deb.sh` is not run by CI.

## Architecture

- **`crates/roc-sys`** — FFI to `libroc` (0.4.x), generated from the
  installed headers by bindgen at build time.
- **`crates/soundnet-protocol`** — JSON types shared with the web UI.
- **`crates/soundnet-engine`** — the daemon:
  - `audio/` — ALSA device enumeration, hardware-parameter negotiation, format conversion
  - `transport/` — roc sender/receiver handles (open/connect/read/write, no threads)
  - `pipeline/` — one thread per route direction, owning both its sound
    device and its roc endpoint. The device's clock paces the loop and roc
    runs `ROC_CLOCK_SOURCE_EXTERNAL`, so there is no buffer — and no second
    clock — between ALSA and the network.
  - `routing.rs` — Route state machine (this-node-plays-src / this-node-plays-dst)
  - `discovery.rs` — mDNS registration + browsing
  - `control/` — axum REST + WebSocket + embedded SPA
  - `tone.rs` — synthetic sine capture ports for preview
- **`web/`** — Vite + React + React Flow patch bay UI

## How this was built

SoundNet was written by ひょうさく together with
[Claude](https://claude.com/claude-code), Anthropic's coding assistant, over a
series of pair-programming sessions. Claude wrote most of the code and the
documentation; every decision, every piece of hardware testing, and every
"that isn't what I heard" that sent a theory back to the drawing board came
from the human side — which is where most of the real findings in this
project came from, since none of the interesting bugs were visible without a
sound card and a pair of ears.

Individual commits carry a `Co-Authored-By: Claude` trailer.

## Licence

MIT — see [LICENSE](LICENSE).

Dependencies and what they ask of you are inventoried in
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md). The short version: nothing
this repo produces contains third-party code except the Rust crates and web
libraries compiled into the binary, all of which are permissive apart from one
MPL-2.0 crate used unmodified. roc-toolkit (MPL-2.0), ALSA (LGPL-2.1+) and
OpenFEC (CeCILL-C) are dynamically linked system libraries and are not
redistributed here — which would change if the `.deb` ever bundled them.
