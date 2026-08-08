//! Roc receiver wrapper: binds local UDP endpoints and pumps decoded samples
//! into a producer ring that the playback side drains.

use anyhow::{anyhow, Result};
use roc_sys as roc;
use rtrb::Producer;
use soundnet_protocol::StreamSpec;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use super::{endpoint_free, endpoint_from_uri, RocContext};

pub struct ReceiverHandle {
    pub stop: Arc<AtomicBool>,
    pub thread: JoinHandle<()>,
}

impl ReceiverHandle {
    pub fn stop_and_join(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.thread.join();
    }
}

pub fn spawn(
    ctx: Arc<RocContext>,
    bind_host: &str,
    bind_port: u16,
    spec: &StreamSpec,
    mut producer: Producer<f32>,
) -> Result<ReceiverHandle> {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_worker = stop.clone();
    let bind_host = bind_host.to_string();
    let spec = spec.clone();

    let thread = thread::Builder::new()
        .name(format!("roc-rx-{bind_host}-{bind_port}"))
        .spawn(move || {
            if let Err(err) = run(&ctx, &bind_host, bind_port, &spec, &mut producer, &stop_worker) {
                tracing::error!("receiver on {bind_host}:{bind_port} failed: {err:#}");
            }
        })?;
    Ok(ReceiverHandle { stop, thread })
}

fn run(
    ctx: &Arc<RocContext>,
    host: &str,
    port: u16,
    spec: &StreamSpec,
    producer: &mut Producer<f32>,
    stop: &Arc<AtomicBool>,
) -> Result<()> {
    let channels = super::sender::channel_layout_for(spec.channels);
    let cfg = roc::roc_receiver_config {
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
        clock_source: roc::roc_clock_source::ROC_CLOCK_SOURCE_INTERNAL,
        clock_sync_backend: roc::roc_clock_sync_backend::ROC_CLOCK_SYNC_BACKEND_DEFAULT,
        clock_sync_profile: roc::roc_clock_sync_profile::ROC_CLOCK_SYNC_PROFILE_DEFAULT,
        resampler_backend: roc::roc_resampler_backend::ROC_RESAMPLER_BACKEND_DEFAULT,
        resampler_profile: roc::roc_resampler_profile::ROC_RESAMPLER_PROFILE_DEFAULT,
        target_latency: (spec.target_latency_ms as u64) * 1_000_000,
        latency_tolerance: 0,
        no_playback_timeout: 0,
        choppy_playback_timeout: 0,
    };

    if matches!(channels, roc::roc_channel_layout::ROC_CHANNEL_LAYOUT_MULTITRACK) {
        let enc = roc::roc_media_encoding {
            rate: spec.rate,
            format: roc::roc_format::ROC_FORMAT_PCM_FLOAT32,
            channels,
            tracks: spec.channels as u32,
        };
        unsafe { roc::roc_context_register_encoding(ctx.raw(), 100, &enc) };
    }

    let mut receiver: *mut roc::roc_receiver = std::ptr::null_mut();
    let rc = unsafe { roc::roc_receiver_open(ctx.raw(), &cfg, &mut receiver) };
    if rc != 0 || receiver.is_null() {
        return Err(anyhow!("roc_receiver_open failed ({rc})"));
    }
    struct DropReceiver(*mut roc::roc_receiver);
    impl Drop for DropReceiver {
        fn drop(&mut self) {
            unsafe { let _ = roc::roc_receiver_close(self.0); }
        }
    }
    let _drop_receiver = DropReceiver(receiver);

    let source_uri = if spec.fec {
        format!("rtp+rs8m://{host}:{port}")
    } else {
        format!("rtp://{host}:{port}")
    };
    let source_ep = endpoint_from_uri(&source_uri)?;
    let rc = unsafe {
        roc::roc_receiver_bind(
            receiver,
            roc::ROC_SLOT_DEFAULT,
            roc::roc_interface::ROC_INTERFACE_AUDIO_SOURCE,
            source_ep,
        )
    };
    endpoint_free(source_ep);
    if rc != 0 {
        return Err(anyhow!("receiver bind source failed ({rc})"));
    }

    if spec.fec {
        let repair_uri = format!("rs8m://{host}:{}", port + 1);
        let repair_ep = endpoint_from_uri(&repair_uri)?;
        let rc = unsafe {
            roc::roc_receiver_bind(
                receiver,
                roc::ROC_SLOT_DEFAULT,
                roc::roc_interface::ROC_INTERFACE_AUDIO_REPAIR,
                repair_ep,
            )
        };
        endpoint_free(repair_ep);
        if rc != 0 {
            return Err(anyhow!("receiver bind repair failed ({rc})"));
        }
    }

    let period_frames = spec.frames_per_period as usize;
    let period_samples = period_frames * spec.channels as usize;
    let mut buf: Vec<f32> = vec![0.0; period_samples];

    while !stop.load(Ordering::Relaxed) {
        let mut frame = roc::roc_frame {
            samples: buf.as_mut_ptr() as *mut _,
            samples_size: buf.len() * std::mem::size_of::<f32>(),
        };
        let rc = unsafe { roc::roc_receiver_read(receiver, &mut frame) };
        if rc != 0 {
            tracing::warn!("roc_receiver_read returned {rc}");
            continue;
        }
        for &s in &buf {
            // Drop if playback fell behind — never block roc.
            let _ = producer.push(s);
        }
    }
    Ok(())
}
