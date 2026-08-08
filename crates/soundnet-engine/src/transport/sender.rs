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
    let channels = channel_layout(spec.channels);
    let mut cfg = roc::roc_sender_config {
        frame_encoding: roc::roc_media_encoding {
            rate: spec.rate,
            format: roc::roc_format::ROC_FORMAT_PCM_FLOAT32,
            channels,
            tracks: if matches!(channels, roc::roc_channel_layout::ROC_CHANNEL_LAYOUT_MULTITRACK) {
                spec.channels as u32
            } else {
                0
            },
        },
        packet_encoding: 0,
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
        resampler_backend: roc::roc_resampler_backend::ROC_RESAMPLER_BACKEND_DEFAULT,
        resampler_profile: roc::roc_resampler_profile::ROC_RESAMPLER_PROFILE_DEFAULT,
    };

    // For non-stereo/mono we must register a custom packet encoding.
    if matches!(channels, roc::roc_channel_layout::ROC_CHANNEL_LAYOUT_MULTITRACK) {
        // Reserve encoding id 100 for our multi-track PCM.
        let enc = roc::roc_media_encoding {
            rate: spec.rate,
            format: roc::roc_format::ROC_FORMAT_PCM_FLOAT32,
            channels,
            tracks: spec.channels as u32,
        };
        // Idempotent — errors when already registered, which is fine.
        unsafe { roc::roc_context_register_encoding(ctx.raw(), 100, &enc) };
        cfg.packet_encoding = 100;
    }

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
        // Wait until at least one period is available; if the capture side is
        // slow, fill silence rather than starve roc (which would drift target
        // latency).
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

pub(super) fn channel_layout_for(ch: u8) -> roc::roc_channel_layout {
    channel_layout(ch)
}

fn channel_layout(ch: u8) -> roc::roc_channel_layout {
    match ch {
        1 => roc::roc_channel_layout::ROC_CHANNEL_LAYOUT_MONO,
        2 => roc::roc_channel_layout::ROC_CHANNEL_LAYOUT_STEREO,
        _ => roc::roc_channel_layout::ROC_CHANNEL_LAYOUT_MULTITRACK,
    }
}
