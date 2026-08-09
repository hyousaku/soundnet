//! Roc sender wrapper: pulls f32 frames from a consumer ring and pushes them
//! to a remote receiver via UDP+RTP (+FEC when enabled).

use anyhow::{anyhow, Result};
use roc_sys as roc;
use rtrb::Consumer;
use soundnet_protocol::StreamSpec;
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
/// samples from `consumer`.
pub fn spawn(
    ctx: Arc<RocContext>,
    remote_host: &str,
    remote_audio_port: u16,
    spec: &StreamSpec,
    consumer: Consumer<f32>,
) -> Result<SenderHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = stop.clone();
    let remote_host = remote_host.to_string();
    let spec = spec.clone();

    let thread = thread::Builder::new()
        .name(format!("roc-tx-{remote_host}"))
        .spawn(move || {
            if let Err(err) = run(&ctx, &remote_host, remote_audio_port, &spec, consumer, &stop_worker) {
                tracing::error!("sender to {remote_host}:{remote_audio_port} failed: {err:#}");
            }
        })?;

    Ok(SenderHandle { stop, thread })
}

fn run(
    ctx: &Arc<RocContext>,
    host: &str,
    audio_port: u16,
    spec: &StreamSpec,
    mut consumer: Consumer<f32>,
    stop: &Arc<AtomicBool>,
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
        packet_length: 0,
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
        latency_tuner_profile: roc::roc_latency_tuner_profile::ROC_LATENCY_TUNER_PROFILE_DEFAULT,
        resampler_backend: roc::roc_resampler_backend::ROC_RESAMPLER_BACKEND_DEFAULT,
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
