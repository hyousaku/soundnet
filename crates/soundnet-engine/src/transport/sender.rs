//! Roc sender: packetises f32 frames and pushes them to a remote receiver
//! via UDP+RTP (+FEC when enabled).
//!
//! This is a handle, not a worker. The thread that owns it is the send
//! pipeline (see `pipeline/send.rs`), which reads a period from ALSA and
//! hands it straight here — there is deliberately no buffer in between, so
//! the sound card's clock is the only clock in the path.
//!
//! The packet payload cap (see `frames_per_packet`) follows the egress
//! interface's MTU rather than assuming Ethernet: `routing::try_start_inner`
//! derives it from whichever interface is pinned and passes it in as
//! `open`'s `max_payload` argument, falling back to
//! `DEFAULT_MAX_PACKET_PAYLOAD_BYTES` when nothing is pinned. The case that
//! motivated this is a tunnel with a smaller MTU than Ethernet's 1500 —
//! Tailscale's `tailscale0` defaults to 1280 — where the old fixed
//! Ethernet-sized cap produced datagrams that fragmented on every hop
//! through the tunnel.

use anyhow::{anyhow, bail, Result};
use core::ffi::c_char;
use roc_sys as roc;
use soundnet_protocol::StreamSpec;
use std::net::IpAddr;
use std::sync::Arc;

use super::{endpoint_free, endpoint_from_uri, RocContext};

/// An open roc sender, connected to one destination.
pub struct Sender {
    raw: *mut roc::roc_sender,
    /// Keeps the shared context alive for at least as long as this sender —
    /// `roc_context_close` on a context with live senders is undefined.
    _ctx: Arc<RocContext>,
}

// SAFETY: `roc_sender` is documented as thread-safe (see roc/sender.h), and
// this only claims the weaker property of being *movable* between threads.
//
// This used to be deliberately `!Send`, on the grounds that confining the
// sender to the one thread that opened it made "the sound card paces
// everything" true by construction. That property is still true and still
// load-bearing, but it is no longer the *open* that establishes it: a capture
// device shared by several routes has its senders opened on the control
// thread — where a socket setup that blocks costs nothing — and moved into
// the device's reading thread, which is then their sole writer for the rest
// of their lives. Ownership moves once; nothing is ever written from two
// threads. See `pipeline/capture.rs`.
unsafe impl Send for Sender {}

impl Drop for Sender {
    fn drop(&mut self) {
        unsafe {
            let _ = roc::roc_sender_close(self.raw);
        }
    }
}

impl Sender {
    /// Hand one period of interleaved f32 samples to roc.
    ///
    /// Non-blocking: the sender is configured `ROC_CLOCK_SOURCE_EXTERNAL`, so
    /// this packetises and returns rather than sleeping until the next
    /// quantum. That is the whole point — the caller has already blocked in
    /// ALSA for exactly one period, and a second clock here would fight it.
    pub fn write(&mut self, samples: &mut [f32]) -> Result<()> {
        let frame = roc::roc_frame {
            samples: samples.as_mut_ptr() as *mut _,
            samples_size: std::mem::size_of_val(samples),
        };
        let rc = unsafe { roc::roc_sender_write(self.raw, &frame) };
        if rc != 0 {
            bail!("roc_sender_write failed ({rc})");
        }
        Ok(())
    }
}

/// Fill a `roc_interface_config` pinning egress to `ip`. `outgoing_address`
/// is a fixed 48-byte NUL-terminated C string field — an IPv4 dotted-quad
/// (max 15 chars) or IPv6 address (max 45) both fit comfortably, but this
/// checks rather than trusts that silently.
fn interface_config_for(ip: IpAddr) -> Result<roc::roc_interface_config> {
    let addr = ip.to_string();
    if addr.len() >= 48 {
        bail!("address {addr} too long for roc_interface_config::outgoing_address");
    }
    let mut outgoing_address = [0 as c_char; 48];
    for (dst, src) in outgoing_address.iter_mut().zip(addr.as_bytes()) {
        *dst = *src as c_char;
    }
    Ok(roc::roc_interface_config {
        outgoing_address,
        multicast_group: [0 as c_char; 48],
        reuse_address: 0,
    })
}

/// Fallback biggest audio payload we let roc put in one packet, in bytes,
/// used when the egress interface's MTU can't be determined (nothing
/// pinned, or the pinned name doesn't currently resolve — see
/// `routing::try_start_inner`).
///
/// Sized so the datagram fits inside a standard 1500-byte Ethernet MTU:
/// 1500 - 40 (IPv6 header; IPv4's 20 only leaves more room) - 8 (UDP)
/// - 12 (RTP) - slack for roc's FEC headers, rounded down to a round number.
///
/// See `max_payload_for_mtu` for the general form this constant is now a
/// special case of.
///
/// Past this the datagram is fragmented by IP, and a fragmented datagram is
/// all-or-nothing: lose either fragment and the whole packet is gone. That is
/// the loss pattern FEC is worst at absorbing, and roc cannot even see it —
/// it counts packets, and the kernel reassembles or discards fragments
/// beneath it. So a fragmented stream degrades in a way that is both harsher
/// and less visible than an unfragmented one.
///
/// This used to be the only cap there was, sized for Ethernet and nothing
/// else. It stopped being safe the day someone ran a route over a Tailscale
/// tunnel: `tailscale0` defaults to a 1280-byte MTU, and this constant alone
/// let a 1400-byte payload become a 1420-byte-plus IP packet that fragments
/// on every hop through the tunnel — invisibly, in exactly the way the
/// paragraph above describes. `max_payload_for_mtu` is what fixed that; this
/// constant now only covers the case where the engine has no MTU to derive
/// the cap from.
pub const DEFAULT_MAX_PACKET_PAYLOAD_BYTES: u32 = 1400;

/// Floor under `max_payload_for_mtu`'s output. Below this the arithmetic
/// (`mtu - 100` for headers) stops being about headroom and starts being
/// about running out of MTU altogether — a pathological or misreported MTU
/// (some tunnel interfaces have reported MTUs in the low hundreds, or even
/// 0, in real deployments) must not be allowed to produce a near-zero or
/// zero-size packet, which would make `frames_per_packet` spin down to 1
/// frame per packet and turn every route into a packet-rate flood.
const MIN_PACKET_PAYLOAD_BYTES: u32 = 128;

/// Derive the packet payload cap from an interface's MTU, following the same
/// budget as `DEFAULT_MAX_PACKET_PAYLOAD_BYTES`: `mtu - 100` for the IP/UDP/
/// RTP/FEC overhead documented there, clamped to `MIN_PACKET_PAYLOAD_BYTES`
/// so a pathological MTU can't produce a degenerate packet size.
///
/// `max_payload_for_mtu(1500) == DEFAULT_MAX_PACKET_PAYLOAD_BYTES` by
/// construction — see the test of the same name, which is what makes this
/// change a no-op on ordinary Ethernet rather than a behaviour change for
/// everyone who was already fine.
pub fn max_payload_for_mtu(mtu: u32) -> u32 {
    mtu.saturating_sub(100).max(MIN_PACKET_PAYLOAD_BYTES)
}

/// Bytes per sample on the wire. The packet encoding registered by
/// `RocContext::ensure_encoding` is `ROC_FORMAT_PCM_FLOAT32`, so four.
const WIRE_BYTES_PER_SAMPLE: u32 = 4;

/// How many frames roc should put in one packet.
///
/// This used to be the ALSA period exactly, which was right for the reason it
/// was chosen — a packet per period adds no chunking delay of its own — and
/// wrong in a way that only shows up with channel count. roc's payload is
/// `frames x channels x 4` bytes, so the period that produces a comfortable
/// 1 KB packet in stereo produces 16 KB at 32 channels. roc allocates packets
/// from a pool of `max_packet_size` (2048 bytes by default), and well before
/// that the datagram stops fitting in an Ethernet frame.
///
/// So the period keeps setting the *latency* and this sets the *packet size*,
/// independently: halve the frames-per-packet until the payload fits. Periods
/// are powers of two, so halving always lands on a whole divisor and each
/// period still ends on a packet boundary — no packet ever straddles two
/// periods, which keeps the "one period in, N whole packets out" property the
/// original choice was after.
///
/// Sending smaller packets sooner cannot add latency; roc emits a packet as
/// soon as it is full, so this only ever makes the first packet leave earlier.
///
/// `max_payload` is the cap to packetise against — Ethernet's 1400 bytes by
/// default, but derived from the egress interface's real MTU when one is
/// pinned (see `max_payload_for_mtu` and `routing::try_start_inner`), because
/// a fixed Ethernet-sized cap is wrong the moment the packets leave over
/// something with a smaller MTU, such as a Tailscale tunnel (`tailscale0` is
/// 1280 by default).
fn frames_per_packet(frames_per_period: u32, channels: u8, max_payload: u32) -> u32 {
    let channels = channels.max(1) as u64;
    let mut frames = frames_per_period.max(1);
    while frames > 1 && frames as u64 * channels * WIRE_BYTES_PER_SAMPLE as u64 > max_payload as u64
    {
        frames /= 2;
    }
    frames
}

/// Convert a frame count to roc's `packet_length` field, which is
/// in nanoseconds rather than frames. Returns 0 — roc's own sentinel for
/// "pick a default" — for inputs that can't produce a sane duration: a zero
/// frame count or rate would otherwise divide into garbage, and a period
/// long enough to exceed a full second is almost certainly a misconfigured
/// `frames_per_period` rather than something worth handing to roc literally.
/// Uses u128 for the intermediate product so a large `frames_per_period`
/// can't silently wrap a 64-bit multiply before the divide brings it back
/// down to a normal nanosecond count.
fn packet_length_ns(frames: u32, rate: u32) -> u64 {
    if frames == 0 || rate == 0 {
        return 0;
    }
    let ns = (frames as u128 * 1_000_000_000u128) / rate as u128;
    if ns == 0 || ns > 1_000_000_000 {
        return 0;
    }
    ns as u64
}

/// Open a sender connected to `host`'s audio port trio. `outgoing` pins the
/// local address packets leave from (a specific NIC's IP); `None` leaves it
/// to the OS routing table. `max_payload` is the packet payload cap to
/// packetise against — see `frames_per_packet` and `max_payload_for_mtu`;
/// callers pass `DEFAULT_MAX_PACKET_PAYLOAD_BYTES` when there is no MTU to
/// derive a better one from.
pub fn open(
    ctx: Arc<RocContext>,
    host: &str,
    audio_port: u16,
    spec: &StreamSpec,
    outgoing: Option<IpAddr>,
    max_payload: u32,
) -> Result<Sender> {
    // Always use MULTITRACK layout — same code path for 1..32 channels, and
    // we register a custom packet encoding below so libroc doesn't try to
    // pick a built-in that doesn't exist for our rate/format.
    let packet_encoding = ctx.ensure_encoding(spec.rate, spec.channels);
    let packet_frames = frames_per_packet(spec.frames_per_period, spec.channels, max_payload);
    // Logged once per open (not per period) at info, unconditionally — so an
    // operator debugging a fragmenting link can find the cap actually in
    // force in the journal instead of inferring it from packet captures.
    // Which interface and MTU (if any) produced this number is logged
    // alongside where those are known — see routing::try_start_inner — this
    // line is what ties that back to a concrete sender.
    tracing::info!("sender {host}:{audio_port}: packet payload cap {max_payload} bytes in force");
    if packet_frames != spec.frames_per_period {
        tracing::info!(
            "sender {host}:{audio_port}: {} channels at period {} would need a \
             {}-byte payload, so packetising {packet_frames} frames at a time \
             ({} packets per period) to stay inside the {max_payload}-byte packet cap",
            spec.channels,
            spec.frames_per_period,
            spec.frames_per_period * spec.channels as u32 * WIRE_BYTES_PER_SAMPLE,
            spec.frames_per_period / packet_frames,
        );
    }
    let cfg = roc::roc_sender_config {
        frame_encoding: roc::roc_media_encoding {
            rate: spec.rate,
            format: roc::roc_format::ROC_FORMAT_PCM_FLOAT32,
            channels: roc::roc_channel_layout::ROC_CHANNEL_LAYOUT_MULTITRACK,
            tracks: spec.channels as u32,
        },
        // The cast is roc's own asymmetry, not ours:
        // `roc_context_register_encoding` takes the identifier as a signed
        // `int`, while the config field that consumes it is declared as the
        // `roc_packet_encoding` enum, whose underlying type is unsigned. Both
        // are 32 bits, and registered ids are small and positive, so this
        // only satisfies the type system. (The field is generated as a plain
        // integer rather than a Rust enum precisely because the value we put
        // in it is never one of that enum's variants — see build.rs.)
        packet_encoding: packet_encoding as roc::roc_packet_encoding::Type,
        // Packetise at or below the ALSA period rather than at roc's own
        // (larger) default quantum — otherwise the network hop adds a
        // chunking delay independent of, and typically bigger than, the
        // period itself. At or *below*: see `frames_per_packet` for why the
        // period alone cannot decide this once a route carries more than a
        // few channels.
        packet_length: packet_length_ns(packet_frames, spec.rate),
        packet_interleaving: 0,
        fec_encoding: if spec.fec {
            roc::roc_fec_encoding::ROC_FEC_ENCODING_RS8M
        } else {
            roc::roc_fec_encoding::ROC_FEC_ENCODING_DISABLE
        },
        fec_block_source_packets: 0,
        fec_block_repair_packets: 0,
        // The capture device's clock drives this pipeline: the thread blocks
        // in `snd_pcm_readi` for exactly one period and then calls
        // `Sender::write`. INTERNAL would add roc's own CPU-timer pacing on
        // top of that, and two clocks with no buffer between them is a
        // guaranteed under/overrun — see the module docs.
        clock_source: roc::roc_clock_source::ROC_CLOCK_SOURCE_EXTERNAL,
        latency_tuner_backend: roc::roc_latency_tuner_backend::ROC_LATENCY_TUNER_BACKEND_DEFAULT,
        // Sender-side latency tuning stays off (target_latency/tolerance are
        // 0 below, which keeps INTACT profile in effect), but we still pin
        // resampler_backend away from DEFAULT/BUILTIN for consistency with
        // the receiver — see the long comment in receiver.rs for why: a
        // roc-toolkit 0.4.0 bug in the builtin resampler can abort() the
        // whole process, and there's no reason to leave that door open on
        // the sender side too if we ever enable sender-side tuning later.
        latency_tuner_profile: roc::roc_latency_tuner_profile::ROC_LATENCY_TUNER_PROFILE_DEFAULT,
        resampler_backend: roc::roc_resampler_backend::ROC_RESAMPLER_BACKEND_SPEEX,
        resampler_profile: roc::roc_resampler_profile::ROC_RESAMPLER_PROFILE_DEFAULT,
        // Latency tuning defaults are managed on the receiver side; keep
        // sender-side tuning disabled by leaving these zero.
        target_latency: 0,
        latency_tolerance: 0,
    };

    let mut raw: *mut roc::roc_sender = std::ptr::null_mut();
    let rc = unsafe { roc::roc_sender_open(ctx.raw(), &cfg, &mut raw) };
    if rc != 0 || raw.is_null() {
        return Err(anyhow!("roc_sender_open failed ({rc})"));
    }
    // Wrap immediately: every early return below has to close the sender,
    // and the Drop impl is the only way to not get that wrong once.
    let sender = Sender { raw, _ctx: ctx };

    // Pin egress before connecting, per interface — source and repair are
    // separate sockets, and an unpinned one would leave repair packets free
    // to go out whichever NIC the OS picks even with the source pinned.
    // libroc treats an empty outgoing_address as "let the OS decide" (the
    // pre-existing behaviour), so this is skipped entirely when nothing's
    // pinned.
    if let Some(ip) = outgoing {
        let iface_cfg = interface_config_for(ip)?;
        let pin = |iface: roc::roc_interface, what: &str| -> Result<()> {
            let rc = unsafe {
                roc::roc_sender_configure(sender.raw, roc::ROC_SLOT_DEFAULT, iface, &iface_cfg)
            };
            if rc != 0 {
                bail!("sender configure {what} outgoing_address failed ({rc})");
            }
            Ok(())
        };
        pin(roc::roc_interface::ROC_INTERFACE_AUDIO_SOURCE, "source")?;
        if spec.fec {
            pin(roc::roc_interface::ROC_INTERFACE_AUDIO_REPAIR, "repair")?;
        }
        // Control (RTCP) is unconditional — unlike repair it doesn't depend
        // on FEC being on, since it carries latency/clock-sync reports, not
        // redundancy packets.
        pin(roc::roc_interface::ROC_INTERFACE_AUDIO_CONTROL, "control")?;
    }

    let connect = |uri: String, iface: roc::roc_interface, what: &str| -> Result<()> {
        let ep = endpoint_from_uri(&uri)?;
        let rc = unsafe { roc::roc_sender_connect(sender.raw, roc::ROC_SLOT_DEFAULT, iface, ep) };
        endpoint_free(ep);
        if rc != 0 {
            bail!("sender connect {what} failed ({rc})");
        }
        Ok(())
    };

    // Source endpoint: rtp+rs8m://host:port (when FEC on) or rtp://host:port.
    let source_uri = if spec.fec {
        format!("rtp+rs8m://{host}:{audio_port}")
    } else {
        format!("rtp://{host}:{audio_port}")
    };
    connect(
        source_uri,
        roc::roc_interface::ROC_INTERFACE_AUDIO_SOURCE,
        "source",
    )?;

    if spec.fec {
        connect(
            format!("rs8m://{host}:{}", audio_port + 1),
            roc::roc_interface::ROC_INTERFACE_AUDIO_REPAIR,
            "repair",
        )?;
    }

    // RTCP control endpoint. Without this, `roc_connection_metrics.e2e_latency`
    // (what the receiver reports as end-to-end latency) is always zero —
    // metrics.h is explicit that it needs RTCP + system clock to compute
    // anything. Always connected, independent of FEC. `audio_port + 2` is
    // the third port in this route's 3-port window; see
    // `routing::route_port` for why the stride between routes has to leave
    // room for it.
    connect(
        format!("rtcp://{host}:{}", audio_port + 2),
        roc::roc_interface::ROC_INTERFACE_AUDIO_CONTROL,
        "control",
    )?;

    Ok(sender)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    /// Every rate/period the UI offers, against every channel count a device
    /// plausibly has. The payload must fit an Ethernet frame in all of them —
    /// that is the property, and checking it exhaustively is cheap enough
    /// that there is no reason to check it by example instead.
    #[test]
    fn no_combination_the_ui_offers_can_fragment_a_datagram() {
        for period in [32u32, 64, 128, 256, 512] {
            for channels in 1..=64u8 {
                let frames = frames_per_packet(period, channels, DEFAULT_MAX_PACKET_PAYLOAD_BYTES);
                let payload = frames as u64 * channels as u64 * WIRE_BYTES_PER_SAMPLE as u64;
                assert!(
                    payload <= DEFAULT_MAX_PACKET_PAYLOAD_BYTES as u64,
                    "period {period} x {channels}ch: {frames} frames is a {payload}-byte payload"
                );
                assert!(
                    frames >= 1,
                    "period {period} x {channels}ch produced no frames"
                );
                assert!(
                    period % frames == 0,
                    "period {period} x {channels}ch: {frames} frames does not divide the \
                     period, so a packet would straddle two of them"
                );
                assert!(
                    frames <= period,
                    "period {period} x {channels}ch: packetising {frames} frames would hold \
                     audio back beyond the period and add latency"
                );
            }
        }
    }

    /// Same sweep, but at the payload cap a Tailscale tunnel actually
    /// produces (`tailscale0`'s default 1280-byte MTU). No combination the UI
    /// offers may produce an IP packet bigger than 1280 bytes — the full
    /// on-wire size, not just the roc payload, since it's the *datagram* that
    /// fragments at the tunnel's MTU. 20 (IPv4) + 8 (UDP) + 12 (RTP) is the
    /// same header budget `max_payload_for_mtu`'s doc comment accounts for.
    #[test]
    fn no_combination_the_ui_offers_can_fragment_a_datagram_over_tailscale() {
        const TAILSCALE_MTU: u32 = 1280;
        let max_payload = max_payload_for_mtu(TAILSCALE_MTU);
        for period in [32u32, 64, 128, 256, 512] {
            for channels in 1..=64u8 {
                let frames = frames_per_packet(period, channels, max_payload);
                let payload = frames as u64 * channels as u64 * WIRE_BYTES_PER_SAMPLE as u64;
                let ip_packet = payload + 12 /* RTP */ + 8 /* UDP */ + 20 /* IPv4 */;
                assert!(
                    ip_packet <= TAILSCALE_MTU as u64,
                    "period {period} x {channels}ch over tailscale0 (mtu {TAILSCALE_MTU}): \
                     {frames} frames is a {ip_packet}-byte IP packet, which fragments on \
                     every hop through the tunnel — the same all-or-nothing loss FEC is \
                     worst at absorbing, and invisible to roc"
                );
                assert!(
                    frames >= 1,
                    "period {period} x {channels}ch over tailscale0 produced no frames"
                );
            }
        }
    }

    /// Pins the regression this change fixes. At the old fixed 1400-byte
    /// Ethernet cap and the recommended 128-frame period, halving the frame
    /// count until the payload fits under 1400 bytes lands on exactly a
    /// 1280-byte payload for 5, 10 and 20 channels alike (64, 32 and 16
    /// frames respectively) — a 1320-byte IP packet, which fragments over
    /// `tailscale0`'s 1280-byte MTU. Power-of-two channel counts (2, 4, 8,
    /// 16, 32) stay at a 1024-byte payload throughout and were never
    /// affected — this test is specifically about the counts that were.
    #[test]
    fn channel_counts_that_fragmented_over_tailscale_are_fixed() {
        const OLD_ETHERNET_CAP: u32 = DEFAULT_MAX_PACKET_PAYLOAD_BYTES;
        const TAILSCALE_MTU: u32 = 1280;
        let tailscale_cap = max_payload_for_mtu(TAILSCALE_MTU);

        for channels in [5u8, 10, 20] {
            let old_frames = frames_per_packet(128, channels, OLD_ETHERNET_CAP);
            let old_payload = old_frames as u64 * channels as u64 * WIRE_BYTES_PER_SAMPLE as u64;
            let old_ip_packet = old_payload + 12 + 8 + 20;
            assert_eq!(
                old_ip_packet, 1320,
                "{channels}ch at the old 1400-byte cap: expected the known 1320-byte \
                 IP packet that fragments over a 1280-MTU tunnel, got {old_ip_packet}"
            );
            assert!(
                old_ip_packet > TAILSCALE_MTU as u64,
                "{channels}ch: {old_ip_packet}-byte packet does not actually exceed the \
                 tunnel MTU, so this isn't the regression being tested"
            );

            let new_frames = frames_per_packet(128, channels, tailscale_cap);
            let new_payload = new_frames as u64 * channels as u64 * WIRE_BYTES_PER_SAMPLE as u64;
            let new_ip_packet = new_payload + 12 + 8 + 20;
            assert!(
                new_ip_packet <= TAILSCALE_MTU as u64,
                "{channels}ch at the MTU-derived cap should no longer fragment over \
                 tailscale0, got a {new_ip_packet}-byte IP packet"
            );
        }
    }

    /// A stereo route at the recommended period already fits, and must be
    /// left exactly as it was — this change is meant to be invisible to the
    /// configurations that were fine.
    #[test]
    fn a_period_that_already_fits_is_left_alone() {
        assert_eq!(
            frames_per_packet(128, 2, DEFAULT_MAX_PACKET_PAYLOAD_BYTES),
            128
        ); // 1024 bytes
        assert_eq!(
            frames_per_packet(64, 4, DEFAULT_MAX_PACKET_PAYLOAD_BYTES),
            64
        ); // 1024 bytes
        assert_eq!(
            frames_per_packet(32, 8, DEFAULT_MAX_PACKET_PAYLOAD_BYTES),
            32
        ); // 1024 bytes
    }

    /// And the configurations that were quietly fragmenting get split. 256
    /// frames of stereo is 2048 bytes, which is over both the Ethernet MTU
    /// and roc's own 2048-byte packet pool — it is what this project was
    /// running when the limit was found.
    #[test]
    fn the_configurations_that_were_fragmenting_get_split() {
        assert_eq!(
            frames_per_packet(256, 2, DEFAULT_MAX_PACKET_PAYLOAD_BYTES),
            128,
            "2048 bytes -> two packets"
        );
        assert_eq!(
            frames_per_packet(128, 8, DEFAULT_MAX_PACKET_PAYLOAD_BYTES),
            32,
            "4096 bytes -> four packets"
        );
        assert_eq!(
            frames_per_packet(128, 32, DEFAULT_MAX_PACKET_PAYLOAD_BYTES),
            8,
            "16384 bytes -> sixteen packets"
        );
    }

    /// Nonsense in must not produce a zero-length packet, which roc reads as
    /// "pick your own default" — silently undoing the whole calculation.
    #[test]
    fn degenerate_input_still_yields_a_usable_packet() {
        assert_eq!(frames_per_packet(0, 2, DEFAULT_MAX_PACKET_PAYLOAD_BYTES), 1);
        assert_eq!(
            frames_per_packet(128, 0, DEFAULT_MAX_PACKET_PAYLOAD_BYTES),
            128
        );
        assert_eq!(
            frames_per_packet(128, 255, DEFAULT_MAX_PACKET_PAYLOAD_BYTES),
            1
        );
        assert!(
            packet_length_ns(
                frames_per_packet(128, 255, DEFAULT_MAX_PACKET_PAYLOAD_BYTES),
                48_000
            ) > 0
        );
    }

    /// This is what makes deriving the cap from the interface MTU a no-op on
    /// ordinary Ethernet: a 1500-byte MTU must reproduce the exact constant
    /// that used to be hardcoded, so every host that was fine before this
    /// change stays fine after it.
    #[test]
    fn mtu_derived_cap_matches_ethernet_default_at_1500() {
        assert_eq!(
            max_payload_for_mtu(1500),
            DEFAULT_MAX_PACKET_PAYLOAD_BYTES,
            "deriving the cap from a standard Ethernet MTU must reproduce the old constant"
        );
    }

    /// Tailscale's `tailscale0` is the case that motivated this: a 1280 MTU
    /// yields an 1180-byte payload cap, well under the old fixed 1400.
    #[test]
    fn mtu_derived_cap_shrinks_for_tailscale() {
        assert_eq!(max_payload_for_mtu(1280), 1180);
    }

    /// A pathologically small or zero MTU must not be allowed to produce a
    /// near-zero or zero-size packet cap — that would make
    /// `frames_per_packet` spin down to one frame per packet regardless of
    /// channel count, turning every route into a packet-rate flood instead
    /// of failing in some more obvious way.
    #[test]
    fn mtu_derived_cap_has_a_floor() {
        assert_eq!(max_payload_for_mtu(0), MIN_PACKET_PAYLOAD_BYTES);
        assert_eq!(max_payload_for_mtu(50), MIN_PACKET_PAYLOAD_BYTES);
        assert_eq!(max_payload_for_mtu(100), MIN_PACKET_PAYLOAD_BYTES);
    }

    /// `outgoing_address` is a fixed 48-byte NUL-terminated C string that
    /// libroc reads directly — this checks the byte-by-byte fill lands the
    /// address correctly and leaves everything after it (the NUL terminator
    /// included) zeroed, rather than trusting that by inspection.
    #[test]
    fn interface_config_encodes_address_and_nul_terminates() {
        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 10, 135));
        let cfg = interface_config_for(ip).expect("well within the 48-byte limit");

        let addr_str = "192.168.10.135";
        for (i, expected) in addr_str.bytes().enumerate() {
            assert_eq!(cfg.outgoing_address[i], expected as c_char, "byte {i}");
        }
        for b in &cfg.outgoing_address[addr_str.len()..] {
            assert_eq!(*b, 0, "bytes after the address must stay zero");
        }
        assert_eq!(cfg.multicast_group, [0 as c_char; 48]);
        assert_eq!(cfg.reuse_address, 0);
    }

    #[test]
    fn packet_length_matches_period_duration() {
        // 128 frames @ 48kHz is the default route spec (StreamSpec::default).
        assert_eq!(packet_length_ns(128, 48_000), 2_666_666);
        // Exact division should come out exact.
        assert_eq!(packet_length_ns(480, 48_000), 10_000_000);
        // Larger period / lower rate, still well under the 1s guard.
        assert_eq!(packet_length_ns(4_410, 44_100), 100_000_000);
    }

    #[test]
    fn packet_length_falls_back_to_default_on_bad_input() {
        assert_eq!(packet_length_ns(0, 48_000), 0, "zero frames");
        assert_eq!(packet_length_ns(128, 0), 0, "zero rate");
        // A period this long is a misconfiguration, not a real packet size.
        assert_eq!(packet_length_ns(u32::MAX, 1), 0, "absurdly long period");
    }
}
