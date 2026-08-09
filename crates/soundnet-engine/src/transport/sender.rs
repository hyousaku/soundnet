//! Roc sender wrapper: pulls f32 frames from a consumer ring and pushes them
//! to a remote receiver via UDP+RTP (+FEC when enabled).

use anyhow::{anyhow, bail, Result};
use core::ffi::c_char;
use roc_sys as roc;
use rtrb::Consumer;
use soundnet_protocol::StreamSpec;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use super::{endpoint_free, endpoint_from_uri, RocContext};

pub struct SenderHandle {
    pub stop: Arc<AtomicBool>,
    pub thread: JoinHandle<()>,
}

impl SenderHandle {
    pub fn stop_and_join(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
    }
}

/// Spawn a roc sender that connects to `remote_host:remote_port` and forwards
/// samples from `consumer`. `outgoing` pins the local address packets are
/// sent from (a specific NIC's IP); `None` leaves it to the OS routing table.
pub fn spawn(
    ctx: Arc<RocContext>,
    remote_host: &str,
    remote_audio_port: u16,
    spec: &StreamSpec,
    consumer: Consumer<f32>,
    outgoing: Option<IpAddr>,
) -> Result<SenderHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = stop.clone();
    let remote_host = remote_host.to_string();
    let spec = spec.clone();

    let thread = thread::Builder::new()
        .name(format!("roc-tx-{remote_host}"))
        .spawn(move || {
            crate::rt::raise_thread_priority("roc sender", crate::rt::PRIO_SENDER);
            if let Err(err) =
                run(&ctx, &remote_host, remote_audio_port, &spec, consumer, &stop_worker, outgoing)
            {
                tracing::error!("sender to {remote_host}:{remote_audio_port} failed: {err:#}");
            }
        })?;

    Ok(SenderHandle { stop, thread })
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

/// Convert a period size in frames to roc's `packet_length` field, which is
/// in nanoseconds rather than frames. Returns 0 — roc's own sentinel for
/// "pick a default" — for inputs that can't produce a sane duration: a zero
/// frame count or rate would otherwise divide into garbage, and a period
/// long enough to exceed a full second is almost certainly a misconfigured
/// `frames_per_period` rather than something worth handing to roc literally.
/// Uses u128 for the intermediate product so a large `frames_per_period`
/// can't silently wrap a 64-bit multiply before the divide brings it back
/// down to a normal nanosecond count.
fn packet_length_ns(frames_per_period: u32, rate: u32) -> u64 {
    if frames_per_period == 0 || rate == 0 {
        return 0;
    }
    let ns = (frames_per_period as u128 * 1_000_000_000u128) / rate as u128;
    if ns == 0 || ns > 1_000_000_000 {
        return 0;
    }
    ns as u64
}

fn run(
    ctx: &Arc<RocContext>,
    host: &str,
    audio_port: u16,
    spec: &StreamSpec,
    mut consumer: Consumer<f32>,
    stop: &Arc<AtomicBool>,
    outgoing: Option<IpAddr>,
) -> Result<()> {
    // Always use MULTITRACK layout — same code path for 1..32 channels, and
    // we register a custom packet encoding below so libroc doesn't try to
    // pick a built-in that doesn't exist for our rate/format.
    let packet_encoding = ctx.ensure_encoding(spec.rate, spec.channels);
    let cfg = roc::roc_sender_config {
        frame_encoding: roc::roc_media_encoding {
            rate: spec.rate,
            format: roc::roc_format::ROC_FORMAT_PCM_FLOAT32,
            channels: roc::roc_channel_layout::ROC_CHANNEL_LAYOUT_MULTITRACK,
            tracks: spec.channels as u32,
        },
        packet_encoding,
        // Match packetisation to the ALSA period instead of leaving it at
        // roc's own (larger) default quantum — otherwise the network hop
        // adds a chunking delay independent of, and typically bigger than,
        // the one we already pay in the ALSA ring.
        packet_length: packet_length_ns(spec.frames_per_period, spec.rate),
        packet_interleaving: 0,
        fec_encoding: if spec.fec {
            roc::roc_fec_encoding::ROC_FEC_ENCODING_RS8M
        } else {
            roc::roc_fec_encoding::ROC_FEC_ENCODING_DISABLE
        },
        fec_block_source_packets: 0,
        fec_block_repair_packets: 0,
        clock_source: roc::roc_clock_source::ROC_CLOCK_SOURCE_INTERNAL,
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

    let mut sender: *mut roc::roc_sender = std::ptr::null_mut();
    let rc = unsafe { roc::roc_sender_open(ctx.raw(), &cfg, &mut sender) };
    if rc != 0 || sender.is_null() {
        return Err(anyhow!("roc_sender_open failed ({rc})"));
    }
    // RAII drop guard so we always close on early return.
    struct DropSender(*mut roc::roc_sender);
    impl Drop for DropSender {
        fn drop(&mut self) {
            unsafe { let _ = roc::roc_sender_close(self.0); }
        }
    }
    let _drop_sender = DropSender(sender);

    // Pin egress before connecting, per interface — source and repair are
    // separate sockets, and an unpinned one would leave repair packets free
    // to go out whichever NIC the OS picks even with the source pinned.
    // libroc treats an empty outgoing_address as "let the OS decide" (the
    // pre-existing behaviour), so this is skipped entirely when nothing's
    // pinned.
    if let Some(ip) = outgoing {
        let iface_cfg = interface_config_for(ip)?;
        let rc = unsafe {
            roc::roc_sender_configure(
                sender,
                roc::ROC_SLOT_DEFAULT,
                roc::roc_interface::ROC_INTERFACE_AUDIO_SOURCE,
                &iface_cfg,
            )
        };
        if rc != 0 {
            return Err(anyhow!("sender configure source outgoing_address failed ({rc})"));
        }
        if spec.fec {
            let rc = unsafe {
                roc::roc_sender_configure(
                    sender,
                    roc::ROC_SLOT_DEFAULT,
                    roc::roc_interface::ROC_INTERFACE_AUDIO_REPAIR,
                    &iface_cfg,
                )
            };
            if rc != 0 {
                return Err(anyhow!("sender configure repair outgoing_address failed ({rc})"));
            }
        }
        // Control (RTCP) is unconditional — unlike repair it doesn't depend
        // on FEC being on, since it carries latency/clock-sync reports, not
        // redundancy packets.
        let rc = unsafe {
            roc::roc_sender_configure(
                sender,
                roc::ROC_SLOT_DEFAULT,
                roc::roc_interface::ROC_INTERFACE_AUDIO_CONTROL,
                &iface_cfg,
            )
        };
        if rc != 0 {
            return Err(anyhow!("sender configure control outgoing_address failed ({rc})"));
        }
    }

    // Source endpoint: rtp+rs8m://host:port (when FEC on) or rtp://host:port.
    let source_uri = if spec.fec {
        format!("rtp+rs8m://{host}:{audio_port}")
    } else {
        format!("rtp://{host}:{audio_port}")
    };
    let source_ep = endpoint_from_uri(&source_uri)?;
    let rc = unsafe {
        roc::roc_sender_connect(
            sender,
            roc::ROC_SLOT_DEFAULT,
            roc::roc_interface::ROC_INTERFACE_AUDIO_SOURCE,
            source_ep,
        )
    };
    endpoint_free(source_ep);
    if rc != 0 {
        return Err(anyhow!("sender connect source failed ({rc})"));
    }

    if spec.fec {
        let repair_uri = format!("rs8m://{host}:{}", audio_port + 1);
        let repair_ep = endpoint_from_uri(&repair_uri)?;
        let rc = unsafe {
            roc::roc_sender_connect(
                sender,
                roc::ROC_SLOT_DEFAULT,
                roc::roc_interface::ROC_INTERFACE_AUDIO_REPAIR,
                repair_ep,
            )
        };
        endpoint_free(repair_ep);
        if rc != 0 {
            return Err(anyhow!("sender connect repair failed ({rc})"));
        }
    }

    // RTCP control endpoint. Without this, `roc_connection_metrics.e2e_latency`
    // (what the receiver reports as end-to-end latency) is always zero —
    // metrics.h is explicit that it needs RTCP + system clock to compute
    // anything. Always connected, independent of FEC. `audio_port + 2` is
    // the third port in this route's 3-port window; see
    // `routing::route_port` for why the stride between routes has to leave
    // room for it.
    let control_uri = format!("rtcp://{host}:{}", audio_port + 2);
    let control_ep = endpoint_from_uri(&control_uri)?;
    let rc = unsafe {
        roc::roc_sender_connect(
            sender,
            roc::ROC_SLOT_DEFAULT,
            roc::roc_interface::ROC_INTERFACE_AUDIO_CONTROL,
            control_ep,
        )
    };
    endpoint_free(control_ep);
    if rc != 0 {
        return Err(anyhow!("sender connect control failed ({rc})"));
    }

    let period_frames = spec.frames_per_period as usize;
    let period_samples = period_frames * spec.channels as usize;
    let mut buf: Vec<f32> = vec![0.0; period_samples];

    while !stop.load(Ordering::Relaxed) {
        // If the capture worker died, its Producer has been dropped and the
        // ring is abandoned — no point streaming silence forever.
        if consumer.is_abandoned() && consumer.slots() == 0 {
            tracing::info!("sender: capture upstream gone, exiting");
            break;
        }
        for slot in buf.iter_mut() {
            *slot = consumer.pop().unwrap_or(0.0);
        }
        let frame = roc::roc_frame {
            samples: buf.as_mut_ptr() as *mut _,
            samples_size: buf.len() * std::mem::size_of::<f32>(),
        };
        let rc = unsafe { roc::roc_sender_write(sender, &frame) };
        if rc != 0 {
            tracing::warn!("roc_sender_write returned {rc}");
        }
    }
    Ok(())
}

// (Channel-layout helpers are no longer needed — we always use MULTITRACK
// with a custom packet encoding registered per (rate, channels) tuple.)

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

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
