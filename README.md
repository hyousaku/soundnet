# SoundNet

Low-latency **LAN audio streaming** between Linux hosts (including
Raspberry Pi 4). One small Rust daemon per host, a shared browser UI you can
open from anywhere on the LAN, patch bay–style routing between USB / built-in
sound cards on different machines, and a target end-to-end latency of ~10 ms
on wired Gigabit.

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

## Runtime dependencies

- Linux kernel with ALSA (any modern distro)
- `libroc` (roc-toolkit) 0.3+  — Debian/Ubuntu/Raspberry Pi OS: `libroc0.3`
- `libasound2` — usually already installed
- (Recommended) `PREEMPT_RT` kernel and membership in the `audio` group for
  real-time scheduling

## Build dependencies

- Rust stable (`rustup default stable`)
- Node.js 20+ and npm (for the web UI)
- `libasound2-dev`, `libroc-dev`, `pkg-config`

Debian/Ubuntu/Raspberry Pi OS:

```bash
sudo apt install libasound2-dev libroc-dev pkg-config build-essential
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# Node via nvm or your distro's package
```

## Build & run

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

```bash
sudo install -m 755 target/release/soundnet-engine /usr/local/bin/
sudo cp packaging/soundnet-engine.service /etc/systemd/system/soundnet-engine@.service
sudo cp packaging/99-realtime.conf /etc/security/limits.d/
sudo systemctl daemon-reload
sudo usermod -aG audio $USER          # log out and back in after this
sudo systemctl enable --now soundnet-engine@$USER.service
```

## Development

```bash
# Run the engine on your dev machine
cargo run -p soundnet-engine

# Separately, run the Vite dev server (proxies /api and /ws to the engine)
cd web && npm run dev
# then open http://localhost:5173
```

Tests:

```bash
cargo test --workspace
```

## Latency tuning

Start with the defaults (48 kHz / S24 / 2 ch / period 128 / target 10 ms /
FEC on) — that's tuned for a USB class-compliant interface on RPi4 with a
target end-to-end latency of ~12–18 ms.

Push it lower:

- Drop `frames_per_period` to 64 or 32 (may xrun on RPi under load)
- Drop `target_latency_ms` to 5 or 3
- Set FEC off if your LAN drops zero packets

Back off:

- Raise `target_latency_ms` to 40–80 if the network is jittery (Wi-Fi, VPN)
- Keep FEC on

All parameters are editable per-route in the UI while audio is running — the
worker restarts transparently.

## Architecture

- **`crates/roc-sys`** — minimal hand-written FFI to `libroc` (0.3.x).
- **`crates/soundnet-protocol`** — JSON types shared with the web UI.
- **`crates/soundnet-engine`** — the daemon:
  - `audio/` — ALSA device enumeration and worker threads
  - `transport/` — roc sender/receiver wrappers
  - `routing.rs` — Route state machine (this-node-plays-src / this-node-plays-dst)
  - `discovery.rs` — mDNS registration + browsing
  - `control/` — axum REST + WebSocket + embedded SPA
  - `tone.rs` — synthetic sine capture ports for preview
- **`web/`** — Vite + React + React Flow patch bay UI

## Licence

MIT.
