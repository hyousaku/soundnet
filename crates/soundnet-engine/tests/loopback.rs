//! End-to-end test through a real ALSA device.
//!
//! Every other test in this repository is a pure function or a fake, which is
//! a strange gap for a project whose hard-won lessons are almost all about
//! ALSA behaviour: `snd_pcm_wait` not starting a prepared stream,
//! `start_threshold`, xrun recovery, the three 24-bit formats. All of those
//! were found by hand on real hardware and are held in place by nothing but
//! comments. This test puts audio through an actual `snd_pcm` and checks that
//! what comes out the other side is the tone that went in.
//!
//! ## Why it is `#[ignore]`d rather than run by CI
//!
//! It needs the `snd-aloop` kernel module — a virtual card whose playback
//! subdevices feed its capture subdevices. GitHub's `ubuntu-24.04` runner
//! cannot load it. Not "it is not loaded": the image has no
//! `/lib/modules/$(uname -r)` tree at all, so there is nothing to load and no
//! `/proc/asound` either. Measured, not assumed — see the commit that added
//! and then removed the probe job.
//!
//! So this runs by hand, on a machine with a Linux kernel that has its
//! modules:
//!
//! ```text
//! sudo modprobe snd-aloop
//! cargo test --test loopback -- --ignored --nocapture
//! ```
//!
//! `#[ignore]` rather than a shell script on purpose: CI still *compiles* it
//! on every push, so it cannot rot into something that no longer matches the
//! API it drives. A script off to one side would quietly stop working and
//! nobody would find out until the day they needed it.
//!
//! ## What it actually covers
//!
//! Tone source → roc sender → UDP on loopback → roc receiver → f32-to-ALSA
//! conversion → `snd_pcm_writei` on a real device, then read back through the
//! kernel and checked in the frequency domain. That is the whole playback
//! path plus the whole transport, with only the capture device faked (by the
//! tone generator, which is a supported source in its own right).
//!
//! It is deliberately not a two-engine test. A second process would add mDNS
//! discovery and its timing to a test whose subject is the audio path, and
//! the self-loop route already sends real RTP through the real receiver — the
//! packets go out of the socket and come back in, they are not short-circuited.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The virtual card `snd-aloop` creates. Playback on device 0 comes out of
/// capture on device 1 (and vice versa), subdevice for subdevice.
const PLAYBACK_DEVICE: &str = "hw:CARD=Loopback,DEV=0";
const CAPTURE_DEVICE: &str = "hw:CARD=Loopback,DEV=1";

const RATE: u32 = 48_000;
const CHANNELS: u32 = 2;
const TONE_HZ: f32 = 440.0;

/// Control and audio ports well away from the defaults, so running this does
/// not collide with an engine already running on the machine.
const CONTROL_PORT: u16 = 17_788;
const AUDIO_PORT: u16 = 20_001;

#[test]
#[ignore = "needs `sudo modprobe snd-aloop`; see the module docs"]
fn a_tone_survives_the_whole_path_to_a_real_device() {
    require_loopback_card();

    let dir = scratch_dir();
    let mut engine = Engine::start(&dir);

    let ports = engine.wait_for_state();
    let tone = ports
        .iter()
        .find(|p| p.kind == "tone")
        .expect("the engine always publishes at least one tone port")
        .clone();
    let playback = ports
        .iter()
        .find(|p| p.kind == "playback" && p.alsa_name == PLAYBACK_DEVICE)
        .unwrap_or_else(|| {
            panic!(
                "the engine did not enumerate {PLAYBACK_DEVICE}; it found: {:?}",
                ports.iter().map(|p| &p.alsa_name).collect::<Vec<_>>()
            )
        })
        .clone();

    engine.add_route(&tone.id, &playback.id);

    // Open the far side of the loopback only once the route is running.
    // snd-aloop makes the two halves share one parameter set, and whichever
    // opens first pins it — opening capture first would constrain the engine
    // instead of observing it, which is the opposite of what this checks.
    let samples = record(Duration::from_millis(600));

    let energy_at = |hz: f32| goertzel(&samples, RATE as f32, hz);
    let at_tone = energy_at(TONE_HZ);
    let neighbours = [220.0, 330.0, 660.0, 880.0, 1000.0]
        .into_iter()
        .map(energy_at)
        .fold(0.0_f32, f32::max);

    engine.stop();
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        at_tone > 0.05,
        "nothing arrived at {TONE_HZ} Hz (energy {at_tone:.6}); the path is silent"
    );
    assert!(
        at_tone > neighbours * 10.0,
        "{TONE_HZ} Hz energy {at_tone:.6} does not dominate the strongest other \
         bin {neighbours:.6} — what came back is not the tone that went in"
    );
}

/// Fail with instructions rather than a puzzle. Running an `#[ignore]`d test
/// is an explicit act, so an unprepared machine deserves to be told exactly
/// what is missing.
fn require_loopback_card() {
    let cards = std::fs::read_to_string("/proc/asound/cards").unwrap_or_default();
    assert!(
        cards.contains("Loopback"),
        "no snd-aloop card found. Run `sudo modprobe snd-aloop` first.\n\
         /proc/asound/cards currently reads:\n{cards}"
    );
}

fn scratch_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("soundnet-loopback-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

#[derive(Clone, Debug, serde::Deserialize)]
struct Port {
    id: String,
    kind: String,
    alsa_name: String,
}

#[derive(serde::Deserialize)]
struct Snapshot {
    self_node: NodeId,
    local_ports: Vec<Port>,
}

#[derive(serde::Deserialize)]
struct NodeId {
    id: String,
}

struct Engine {
    child: Child,
    node_id: String,
}

impl Engine {
    fn start(dir: &Path) -> Self {
        // A config of its own, so the test never reads or writes the config
        // of an engine actually in use on this machine.
        let child = Command::new(env!("CARGO_BIN_EXE_soundnet-engine"))
            .arg("--bind")
            .arg(format!("127.0.0.1:{CONTROL_PORT}"))
            .arg("--audio-port")
            .arg(AUDIO_PORT.to_string())
            .arg("--config")
            .arg(dir.join("config.toml"))
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn the engine binary");
        Engine {
            child,
            node_id: String::new(),
        }
    }

    fn wait_for_state(&mut self) -> Vec<Port> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(status) = self.child.try_wait().expect("try_wait") {
                panic!("the engine exited before serving any state ({status})");
            }
            match ureq::get(&format!("http://127.0.0.1:{CONTROL_PORT}/api/state")).call() {
                Ok(resp) => {
                    let snap: Snapshot = resp.into_json().expect("parse /api/state");
                    self.node_id = snap.self_node.id;
                    return snap.local_ports;
                }
                Err(err) if Instant::now() < deadline => {
                    let _ = err;
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(err) => panic!("engine never answered /api/state: {err}"),
            }
        }
    }

    fn add_route(&self, src_port: &str, dst_port: &str) {
        let body = serde_json::json!({
            "id": "loopback-test",
            "src": { "node_id": self.node_id, "port_id": src_port, "channel_offset": 0 },
            "dst": { "node_id": self.node_id, "port_id": dst_port, "channel_offset": 0 },
            "spec": {
                "encoding": { "kind": "pcm" },
                "rate": RATE,
                "channels": CHANNELS,
                "frames_per_period": 256,
                // Pinned rather than left to the fallback chain: snd-aloop
                // makes both halves share one format, and this test has to
                // open the capture side with whatever the engine chose. S16
                // is the one every aloop build accepts.
                "alsa_format": "S16_LE",
                "target_latency_ms": 40,
                "fec": false
            }
        });
        let resp =
            ureq::post(&format!("http://127.0.0.1:{CONTROL_PORT}/api/routes")).send_json(body);
        match resp {
            Ok(r) => assert_eq!(r.status(), 201, "unexpected status creating the route"),
            Err(ureq::Error::Status(code, r)) => {
                let mut body = String::new();
                let _ = r.into_reader().read_to_string(&mut body);
                panic!("creating the route failed with {code}: {body}");
            }
            Err(err) => panic!("creating the route failed: {err}"),
        }
    }

    fn stop(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Read from the capture half of the loopback, discarding the beginning.
///
/// The discard is not padding for its own sake: the receive pipeline ramps
/// audio in from silence on a freshly opened route (`COLD_RESUME_FADE_MS`,
/// two seconds), so the first samples out of the device are genuinely quiet
/// by design. Measuring them would test the fade, not the path.
fn record(window: Duration) -> Vec<f32> {
    use alsa::pcm::{Access, Format, HwParams};

    let pcm = open_capture();
    {
        let hwp = HwParams::any(&pcm).expect("hw params");
        hwp.set_channels(CHANNELS).expect("channels");
        hwp.set_rate(RATE, alsa::ValueOr::Nearest).expect("rate");
        hwp.set_format(Format::s16()).expect("format");
        hwp.set_access(Access::RWInterleaved).expect("access");
        pcm.hw_params(&hwp).expect("apply hw params");
    }
    let io = pcm.io_i16().expect("io");
    pcm.start().expect("start capture");

    let frames_to_skip = (RATE as u64 * 2_500 / 1000) as usize;
    let frames_to_keep = (RATE as u64 * window.as_millis() as u64 / 1000) as usize;

    let mut buf = vec![0i16; 1024 * CHANNELS as usize];
    let mut skipped = 0usize;
    let mut out: Vec<f32> = Vec::with_capacity(frames_to_keep);
    let deadline = Instant::now() + Duration::from_secs(20);

    while out.len() < frames_to_keep {
        assert!(
            Instant::now() < deadline,
            "capture never delivered enough audio"
        );
        let frames = match io.readi(&mut buf) {
            Ok(n) => n,
            Err(err) => {
                pcm.try_recover(err, false).expect("recover capture");
                continue;
            }
        };
        for frame in buf[..frames * CHANNELS as usize].chunks_exact(CHANNELS as usize) {
            if skipped < frames_to_skip {
                skipped += 1;
                continue;
            }
            if out.len() < frames_to_keep {
                // Left channel only: the tone is the same on both, and mixing
                // them would let one silent channel mask a real failure.
                out.push(frame[0] as f32 / i16::MAX as f32);
            }
        }
    }
    out
}

/// Open the capture side, retrying while the engine is still bringing the
/// playback side up. snd-aloop refuses a capture open until its paired
/// playback substream exists, so an EBUSY/ENODEV here early on means "not
/// yet", not "broken".
fn open_capture() -> alsa::pcm::PCM {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match alsa::pcm::PCM::new(CAPTURE_DEVICE, alsa::Direction::Capture, false) {
            Ok(pcm) => return pcm,
            Err(err) if Instant::now() < deadline => {
                let _ = err;
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(err) => panic!("could not open {CAPTURE_DEVICE}: {err}"),
        }
    }
}

/// Energy at one frequency, normalized so a full-scale sine reads about 0.5.
///
/// A Goertzel rather than an FFT because this needs exactly six bins and
/// pulling in a transform crate to get them would be more dependency than
/// arithmetic. Comparing the target bin against several others is what makes
/// this a real check: a DC offset, a burst of noise or a stuck buffer all
/// raise every bin, and only a tone raises one.
fn goertzel(samples: &[f32], rate: f32, hz: f32) -> f32 {
    let n = samples.len() as f32;
    let coeff = 2.0 * (std::f32::consts::TAU * hz / rate).cos();
    let (mut s1, mut s2) = (0.0_f32, 0.0_f32);
    for &x in samples {
        let s0 = x + coeff * s1 - s2;
        s2 = s1;
        s1 = s0;
    }
    let power = s1 * s1 + s2 * s2 - coeff * s1 * s2;
    2.0 * power.max(0.0).sqrt() / n
}
