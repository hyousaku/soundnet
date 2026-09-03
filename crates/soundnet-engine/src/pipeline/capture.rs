//! One ALSA capture device, read by one thread, fanned out to every route
//! that subscribes to it.
//!
//! A capture device can only be opened once. Before this module, a route
//! owned its device outright, so pointing a second route at the same input
//! failed `snd_pcm_open` with EBUSY and retried forever — and the error said
//! "Device or resource busy" without mentioning that the thing holding it
//! was this same engine's other route. Meanwhile "send channels 1-2 to one
//! machine and 3-4 to another" is an entirely ordinary thing to want from a
//! multichannel interface.
//!
//! So the device gets an owner, and routes subscribe to it. See
//! `docs/capture-sharing.md` for the design and the alternatives that were
//! rejected.
//!
//! ## The invariants this has to preserve
//!
//! This loop used to live in `send.rs`, one copy per route, and the reasoning
//! recorded there governs it still.
//!
//! **The device's clock is the only clock.** This was once two threads with a
//! lock-free ring between them. That ring was pure latency — three periods of
//! it — and worse, it hid a real problem: the ALSA side was paced by the
//! sound card while the roc side paced itself off a CPU timer
//! (`ROC_CLOCK_SOURCE_INTERNAL`). Two clocks that do not agree means the ring
//! drifts towards full or empty, and both ends "handled" that by silently
//! dropping samples or substituting silence. Now one period is read and
//! handed straight to every subscriber's sender with nothing buffered in
//! between, so there is nothing to drift — and that stays true however many
//! subscribers there are, because they all ride the same read. Fanning out
//! through a ring would bring the whole problem back for no gain.
//!
//! **Exactly one blocking point per iteration**, because this runs
//! `SCHED_FIFO` (see `rt.rs`) and a real-time thread that never blocks pins a
//! core at 100% and starves everything below it. `snd_pcm_wait` is that
//! point. Draining the command channel is a `try_recv`, and
//! `roc_sender_write` under `ROC_CLOCK_SOURCE_EXTERNAL` packetizes and
//! returns without sleeping, so N of them add no second place to block.
//!
//! The wait carries a timeout rather than blocking in `snd_pcm_readi`
//! directly, which is what bounds teardown: `readi` returns when the device
//! says so, so a device that stopped answering entirely — USB pulled
//! mid-stream, driver stuck in D state — used to keep its thread for the life
//! of the process. A wait that returns ready means at least `avail_min`
//! frames are queued, and `avail_min` defaults to one period, so the `readi`
//! after it is served from what is already there. The subtle cost is that
//! `snd_pcm_wait` only *observes* the device while `snd_pcm_readi` also
//! *starts* one — see `pcm::ensure_capture_running` for what that quietly
//! used to do for us after every xrun recovery.
//!
//! ## How subscribers arrive and leave
//!
//! Through a channel, not a shared lock. A `Mutex<Vec<Subscriber>>` cannot
//! work here: the audio thread would have to hold the lock to reach the
//! senders at all, so "take it if you can, otherwise use last time's
//! snapshot" is not available — the senders *are* the state. That leaves
//! blocking on the lock every period, which invites priority inversion on a
//! real-time thread for the sake of a list that changes when an operator
//! clicks something.
//!
//! Instead the control side builds a `Subscriber` completely — including
//! opening its roc sender, which does socket setup and must not happen on
//! the audio thread — and sends it down a channel. The audio thread owns the
//! list outright and drains the channel once per iteration.
//!
//! One wart, stated rather than hidden: removing a subscriber drops its
//! `Sender` on the audio thread, so `roc_sender_close` runs there. The
//! alternative is handing the corpse back over a return channel, which is
//! tidier for the real-time thread and leaks an open socket for every
//! removal the control side ever forgets to drain. A single close during a
//! route removal — an operator action, and already the noisiest moment this
//! system has — is the smaller problem.

use anyhow::{bail, Result};
use soundnet_protocol::{SampleFormat, StreamSpec, UNKNOWN_FORMAT};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};

use crate::audio::format::alsa_to_f32;
use crate::audio::{pcm, window};
use crate::pipeline::fade::Fade;
use crate::pipeline::{
    publish_level, DEVICE_WAIT_TIMEOUT_MS, MAX_CONSECUTIVE_ERRORS, RESUME_FADE_MS, STALL_WARN_AFTER,
};
use crate::transport::sender;

/// The hardware parameters a capture device is opened with.
///
/// Every route sharing a device must agree on these, because the device is
/// opened once and there is only one answer to give it. Channel window,
/// destination, target latency and FEC stay per-route — those are the
/// sender's business, not the card's.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DeviceParams {
    pub rate: u32,
    pub frames_per_period: u32,
    pub alsa_format: SampleFormat,
}

impl DeviceParams {
    pub fn of(spec: &StreamSpec) -> Self {
        Self {
            rate: spec.rate,
            frames_per_period: spec.frames_per_period,
            alsa_format: spec.alsa_format,
        }
    }
}

impl std::fmt::Display for DeviceParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}Hz/{}f/{:?}",
            self.rate, self.frames_per_period, self.alsa_format
        )
    }
}

/// One route's health on a shared device, shared with its `SendHandle`.
///
/// A device can outlive one of its routes: if a destination's sender fails
/// over and over, the audio thread drops that subscriber and keeps reading
/// for everybody else. Without somewhere to say so, the dropped route would
/// sit in the UI as a healthy "ok" producing silence, because the device
/// thread is alive, nothing has stalled and no xrun was counted — the same
/// dishonest green this project has spent a lot of effort removing
/// everywhere else.
#[derive(Default)]
pub struct SubscriberHealth {
    failed: AtomicBool,
    reason: Mutex<Option<String>>,
}

impl SubscriberHealth {
    pub fn has_failed(&self) -> bool {
        self.failed.load(Ordering::Relaxed)
    }

    pub fn reason(&self) -> Option<String> {
        self.reason
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    fn fail(&self, reason: String) {
        // Poisoning ignored on purpose — see the doc on
        // `ToneWorker::last_error` in `send.rs`.
        *self.reason.lock().unwrap_or_else(|e| e.into_inner()) = Some(reason);
        // Written after the reason so a reader that sees the flag always
        // finds the explanation already there.
        self.failed.store(true, Ordering::Release);
    }
}

/// One destination fed from a capture device.
struct Subscriber {
    route_id: String,
    sender: sender::Sender,
    /// Where in the device's frame this route's channels start, and how many
    /// it takes.
    channel_offset: usize,
    channels: usize,
    level_bits: Arc<AtomicU32>,
    health: Arc<SubscriberHealth>,
    /// Scratch for this subscriber's extracted window, so the audio thread
    /// never allocates inside the loop.
    floats: Vec<f32>,
    /// Counted per subscriber, not per device: a destination that has gone
    /// bad must not be able to take the device — and everybody else on it —
    /// down with it.
    consecutive_errors: u32,
}

impl Subscriber {
    fn window_width(&self) -> usize {
        self.channel_offset + self.channels
    }
}

/// What a route gets back for joining a device.
pub struct Subscription {
    pub owner: Arc<CaptureOwner>,
    /// Per route, not per device: two routes off one interface usually carry
    /// different channels of it, and a meter driven by the wrong ones would
    /// be worse than no meter.
    pub level_bits: Arc<AtomicU32>,
    pub health: Arc<SubscriberHealth>,
    /// Routes whose audio this subscription briefly interrupted, because the
    /// device had to be reopened wider to fit it. Empty in every other case.
    /// The caller tells the operator; a gap nobody explains reads as a fault.
    pub interrupted: Vec<String>,
}

enum Command {
    Add(Box<Subscriber>),
    Remove(String),
}

/// A capture device and the thread reading it.
pub struct CaptureOwner {
    pub alsa_name: String,
    /// What the device was asked for. Compared against later routes rather
    /// than the *negotiated* format, because two routes requesting the same
    /// thing will negotiate the same thing, and the negotiated value is not
    /// known until the thread has opened the card.
    params: DeviceParams,
    /// How many channels the device is currently open with — the far edge of
    /// the widest window any of its routes asks for. Grows when a route needs
    /// channels past it; see `reopen_wider`.
    device_channels: Mutex<usize>,
    stop: Arc<AtomicBool>,
    thread: Mutex<Option<JoinHandle<()>>>,
    commands: Mutex<mpsc::Sender<Command>>,
    /// Where the reading thread leaves its subscribers when it exits, so a
    /// reopen can carry them across without rebuilding their roc senders. A
    /// rebuilt sender would be a new RTP session to the far end, which its
    /// receiver treats as a stream that went away and came back — two seconds
    /// of cold-resume ramp on a gap that was tens of milliseconds.
    handover: Arc<Mutex<Option<Vec<Subscriber>>>>,
    /// Set while the device is being reopened. `is_dead` reports healthy in
    /// that window: the old thread really has gone, but the route is not
    /// failing — it is a few milliseconds from having a wider device — and
    /// letting the supervisor see "dead" would tear down every route on the
    /// card to fix a gap that was about to close by itself.
    reopening: AtomicBool,
    /// Control-side mirror of who is subscribed. The audio thread has its own
    /// list; this one exists so a rejected route can be told which route is
    /// already holding the device, and so the registry knows when the last
    /// subscriber has gone.
    roster: Mutex<Vec<String>>,

    // Device-level published state. Shared by every route on this device,
    // because these are properties of the card and not of any one route.
    pub xruns: Arc<AtomicUsize>,
    pub buffer_ns: Arc<AtomicU64>,
    pub format: Arc<AtomicU8>,
    pub stalled: Arc<AtomicBool>,
    pub last_error: Arc<Mutex<Option<String>>>,

    /// Kept so a reopen can hand the same published state to the new thread —
    /// every route's `SendHandle` already holds these `Arc`s, so replacing
    /// them would freeze the stats of every route on the device.
    worker: Worker,
    spec: StreamSpec,
}

impl CaptureOwner {
    fn start(alsa_name: &str, spec: &StreamSpec, first: Subscriber) -> Result<Arc<Self>> {
        let device_channels = first.window_width();
        let stop = Arc::new(AtomicBool::new(false));
        let xruns = Arc::new(AtomicUsize::new(0));
        let buffer_ns = Arc::new(AtomicU64::new(u64::MAX));
        let format = Arc::new(AtomicU8::new(UNKNOWN_FORMAT));
        let stalled = Arc::new(AtomicBool::new(false));
        let last_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let handover: Arc<Mutex<Option<Vec<Subscriber>>>> = Arc::new(Mutex::new(None));

        let worker = Worker {
            stop: stop.clone(),
            xruns: xruns.clone(),
            buffer_ns: buffer_ns.clone(),
            format: format.clone(),
            stalled: stalled.clone(),
        };

        let owner = Arc::new(Self {
            alsa_name: alsa_name.to_string(),
            params: DeviceParams::of(spec),
            device_channels: Mutex::new(device_channels),
            stop,
            thread: Mutex::new(None),
            // Replaced immediately by `spawn_reader`; a channel with no
            // receiver is the honest placeholder for "no thread yet".
            commands: Mutex::new(mpsc::channel().0),
            handover,
            reopening: AtomicBool::new(false),
            roster: Mutex::new(vec![first.route_id.clone()]),
            xruns,
            buffer_ns,
            format,
            stalled,
            last_error,
            worker,
            spec: spec.clone(),
        });
        owner.spawn_reader(vec![first], device_channels)?;
        Ok(owner)
    }

    /// Start (or restart) the reading thread with `subscribers` and a device
    /// opened for `device_channels`.
    fn spawn_reader(&self, subscribers: Vec<Subscriber>, device_channels: usize) -> Result<()> {
        let (tx, rx) = mpsc::channel();
        self.stop.store(false, Ordering::Relaxed);

        let worker = self.worker.clone();
        let error_worker = self.last_error.clone();
        let handover = self.handover.clone();
        let name = self.alsa_name.clone();
        let spec = self.spec.clone();

        let thread = thread::Builder::new()
            .name(format!("capture-{}", self.alsa_name))
            .spawn(move || {
                crate::rt::raise_thread_priority("capture device", crate::rt::PRIO_SEND);
                let mut subscribers = subscribers;
                let result =
                    owner_loop(&name, &spec, device_channels, &worker, &mut subscribers, rx);
                // Hand the subscribers back whatever happened, so a reopen can
                // reuse them. On a failure nobody collects them and they are
                // dropped with the owner, which closes their senders — the
                // same as before.
                *handover.lock().unwrap_or_else(|e| e.into_inner()) = Some(subscribers);
                if let Err(err) = result {
                    tracing::error!("capture device {name} failed: {err:#}");
                    // Poisoning ignored on purpose — see the doc on
                    // `ToneWorker::last_error` in `send.rs`.
                    *error_worker.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(format!("{err:#}"));
                }
            })?;

        *self.commands.lock().unwrap_or_else(|e| e.into_inner()) = tx;
        *self.thread.lock().unwrap_or_else(|e| e.into_inner()) = Some(thread);
        *self
            .device_channels
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = device_channels;
        Ok(())
    }

    /// Stop the reader, reopen the card wide enough for `device_channels`, and
    /// carry every existing route across along with `extra`.
    ///
    /// This is the one operation here that deliberately interrupts audio
    /// somebody else is listening to, and it was a decision taken with the
    /// operator rather than by the code: reopening is what makes "patch
    /// channels 5-6 somewhere" work at all, and the alternatives were
    /// refusing (which forces the operator to delete and recreate the other
    /// route by hand — the same gap, plus the manual steps) or always opening
    /// every channel the card has, which spends USB bandwidth and CPU on
    /// every period forever to avoid a gap that happens when somebody clicks.
    ///
    /// The gap is bounded by one device teardown and reopen, and the existing
    /// roc senders are carried across untouched — a rebuilt sender would look
    /// to the far end like the stream vanished and returned, which earns a
    /// two-second cold-resume ramp for a gap of tens of milliseconds.
    fn reopen_wider(&self, device_channels: usize, extra: Subscriber) -> Result<()> {
        // Set before the teardown and cleared after the respawn, so nothing
        // sampling `is_dead` in between mistakes a deliberate reopen for a
        // device that failed.
        self.reopening.store(true, Ordering::Release);
        self.stop_and_join();

        let mut subscribers = self
            .handover
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap_or_default();
        subscribers.push(extra);

        let result = self.spawn_reader(subscribers, device_channels);
        self.reopening.store(false, Ordering::Release);
        result
    }

    /// Route ids currently subscribed, for error messages.
    pub fn roster(&self) -> Vec<String> {
        self.roster
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// How many channels the device is currently open with.
    pub fn device_channels(&self) -> usize {
        *self
            .device_channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Whether the reading thread has stopped. A dead owner means every route
    /// on the device is dead, which is exactly right: they all depended on
    /// that one read.
    pub fn is_dead(&self) -> bool {
        if self.reopening.load(Ordering::Acquire) {
            return false;
        }
        self.thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map(|t| t.is_finished())
            .unwrap_or(true)
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    fn stop_and_join(&self) {
        self.request_stop();
        if let Some(t) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = t.join();
        }
    }

    /// Why the device stopped, as reported by its own thread.
    pub fn failure_reason(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// Device-level state the reading thread publishes.
#[derive(Clone)]
struct Worker {
    stop: Arc<AtomicBool>,
    xruns: Arc<AtomicUsize>,
    buffer_ns: Arc<AtomicU64>,
    format: Arc<AtomicU8>,
    stalled: Arc<AtomicBool>,
}

/// Every capture device this engine currently has open, keyed by ALSA name.
///
/// One registry rather than a map on `EngineState` directly, because
/// subscribing has to be atomic with respect to creating: two routes on the
/// same device arriving together must not both decide the device is not open
/// yet. `routing`'s per-route lock cannot cover that — the two routes have
/// different locks.
#[derive(Default)]
pub struct CaptureRegistry {
    owners: Mutex<HashMap<String, Arc<CaptureOwner>>>,
}

impl CaptureRegistry {
    /// Attach a route to `alsa_name`, opening the device if this is the first
    /// one on it.
    ///
    /// `build` is called only once the device is known to be compatible, so a
    /// route that is going to be refused does not pay for opening a roc
    /// sender and a UDP socket first.
    pub fn subscribe(
        &self,
        alsa_name: &str,
        spec: &StreamSpec,
        route_id: &str,
        channel_offset: usize,
        channels: usize,
        build: impl FnOnce() -> Result<sender::Sender>,
    ) -> Result<Subscription> {
        let mut owners = self.owners.lock().unwrap_or_else(|e| e.into_inner());

        // A device whose thread has died is not usable and not worth waiting
        // for: drop it and open a fresh one. Its subscribers are all dead too
        // and the supervisor will retry them.
        if let Some(existing) = owners.get(alsa_name) {
            if existing.is_dead() {
                let dead = owners.remove(alsa_name).expect("just looked it up");
                dead.stop_and_join();
            }
        }

        // Built here rather than inside the subscriber so the caller gets
        // handles on them — these are the two things that stay per route even
        // when the device is shared.
        let level_bits = Arc::new(AtomicU32::new(0));
        let health = Arc::new(SubscriberHealth::default());
        let make_subscriber = |sender| Subscriber {
            route_id: route_id.to_string(),
            sender,
            channel_offset,
            channels,
            level_bits: level_bits.clone(),
            health: health.clone(),
            floats: Vec::with_capacity(spec.frames_per_period as usize * channels),
            consecutive_errors: 0,
        };

        if let Some(owner) = owners.get(alsa_name).cloned() {
            let width = channel_offset + channels;
            let fit = check_compatible(&owner, spec, width)?;
            let sub = make_subscriber(build()?);
            let interrupted = owner.roster();
            owner
                .roster
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push(route_id.to_string());

            match fit {
                Fit::NeedsWider => {
                    // Reopening interrupts the routes already on this card, so
                    // say who and why. Silence here would look like a glitch;
                    // this way the operator can connect the dip to the thing
                    // they just did.
                    tracing::warn!(
                        "capture {alsa_name}: reopening for {width} channels to fit route \
                         {route_id}; this briefly interrupts route {}",
                        interrupted.join(", ")
                    );
                    if let Err(err) = owner.reopen_wider(width, sub) {
                        owner
                            .roster
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .retain(|id| id != route_id);
                        return Err(err);
                    }
                }
                Fit::AsIs => {
                    if owner
                        .commands
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .send(Command::Add(Box::new(sub)))
                        .is_err()
                    {
                        // The device died between the liveness check above and
                        // here. Take the roster entry back out: leaving it
                        // would make the next rejected route name a route that
                        // is not running.
                        owner
                            .roster
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .retain(|id| id != route_id);
                        bail!("capture device {alsa_name} stopped while adding this route");
                    }
                }
            }

            tracing::info!(
                "route {route_id} joined capture {alsa_name}, now serving {} routes",
                owner.roster().len()
            );
            return Ok(Subscription {
                owner,
                level_bits,
                health,
                interrupted: match fit {
                    Fit::NeedsWider => interrupted,
                    Fit::AsIs => Vec::new(),
                },
            });
        }

        let owner = CaptureOwner::start(alsa_name, spec, make_subscriber(build()?))?;
        owners.insert(alsa_name.to_string(), owner.clone());
        Ok(Subscription {
            owner,
            level_bits,
            health,
            interrupted: Vec::new(),
        })
    }

    /// Detach a route. The device stays open until its last route leaves.
    pub fn unsubscribe(&self, alsa_name: &str, route_id: &str) {
        let mut owners = self.owners.lock().unwrap_or_else(|e| e.into_inner());
        let Some(owner) = owners.get(alsa_name).cloned() else {
            return;
        };

        let remaining = {
            let mut roster = owner.roster.lock().unwrap_or_else(|e| e.into_inner());
            roster.retain(|id| id != route_id);
            roster.len()
        };

        if remaining > 0 {
            let _ = owner
                .commands
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .send(Command::Remove(route_id.to_string()));
            tracing::info!(
                "route {route_id} left capture {alsa_name}, still serving {remaining} routes"
            );
            return;
        }

        // Last one out closes the device. Joining here rather than detaching
        // is what makes "the route stopped" mean "the card is free" — without
        // it, a route restarted immediately after a stop would race its own
        // predecessor for the hardware.
        owners.remove(alsa_name);
        drop(owners);
        owner.stop_and_join();
    }

    /// Raise the stop flag on every device without waiting for any of them,
    /// so a whole-engine shutdown pays one device's teardown rather than the
    /// sum of all of them.
    pub fn request_stop_all(&self) {
        for owner in self
            .owners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            owner.request_stop();
        }
    }
}

/// How a route can join a device that is already open.
enum Fit {
    /// The device is open wide enough; just attach.
    AsIs,
    /// Same hardware parameters, but this route needs channels past the far
    /// edge of what the card is currently open for. The device has to be
    /// reopened, which interrupts everything already on it.
    NeedsWider,
}

/// Whether a route can join a device that is already open, and a message
/// naming the route in the way if not.
///
/// The same shape as the port-collision error in `routing`: the operator's
/// question is always "which of my routes is fighting this one", and the
/// engine is the only thing that can answer it.
fn check_compatible(owner: &CaptureOwner, spec: &StreamSpec, window_width: usize) -> Result<Fit> {
    let wanted = DeviceParams::of(spec);
    if wanted != owner.params {
        bail!(
            "capture {} is already open at {} for route {}; this route asks for {}. \
             A device can only be opened once, so routes sharing an input must agree on \
             rate, period and format — change one of them to match, or use a different input.",
            owner.alsa_name,
            owner.params,
            owner.roster().join(", "),
            wanted
        );
    }
    if window_width > owner.device_channels() {
        return Ok(Fit::NeedsWider);
    }
    Ok(Fit::AsIs)
}

/// Largest absolute sample in a period.
///
/// Deliberately not also counting samples past full scale the way the playback
/// side does: what arrives here has already been through the interface's
/// converter, so anything clipped was clipped in hardware before this engine
/// saw it, and a counter here would name SoundNet as the culprit for something
/// it cannot see and did not do. How close the signal runs to the rails is the
/// part we can report honestly, and that is what the meter shows.
fn peak_of(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0_f32, |acc, s| acc.max(s.abs()))
}

fn owner_loop(
    alsa_name: &str,
    spec: &StreamSpec,
    device_channels: usize,
    w: &Worker,
    subscribers: &mut Vec<Subscriber>,
    commands: mpsc::Receiver<Command>,
) -> Result<()> {
    let (pcm, format) = pcm::open(
        alsa_name,
        alsa::Direction::Capture,
        spec,
        device_channels as u32,
    )?;
    w.format.store(format.as_u8(), Ordering::Relaxed);
    let io = pcm.io_bytes();
    // Not an optimization any more: the loop below waits before it reads, and
    // a wait on a prepared-but-not-started capture stream never returns data.
    // See `pcm::ensure_capture_running`.
    pcm::ensure_capture_running(&pcm);

    let frame_bytes = device_channels * format.bytes_per_sample();
    let period_frames = spec.frames_per_period as usize;
    let mut raw = vec![0u8; period_frames * frame_bytes];
    // Everything the device gives us; each subscriber then takes its own
    // window out of it.
    let mut device_floats: Vec<f32> = Vec::with_capacity(period_frames * device_channels);

    let metrics_every = pcm::metrics_every(spec.rate, period_frames);
    let mut ticks = 0_usize;
    let mut consecutive_errors = 0_u32;
    let mut stalled = 0_u32;

    // Ramp in from silence whenever this device starts producing, which means
    // at open and again after every xrun recovery. Both are moments when the
    // driver has just (re)started its DMA ring, and the first period or two
    // out of a freshly started ring is not reliably audio — on some hardware
    // it is whatever was in that memory. Shipping that at full scale is one
    // of the ways a remote machine's speakers get a bang out of nowhere.
    let mut fade = Fade::new(spec.rate);
    fade.arm(RESUME_FADE_MS);

    while !w.stop.load(Ordering::Relaxed) {
        // Non-blocking, so this does not become a second place to block. A
        // `Remove` drops the subscriber's roc sender here on the audio
        // thread; see the module docs for why that is the lesser evil.
        while let Ok(cmd) = commands.try_recv() {
            match cmd {
                Command::Add(sub) => {
                    tracing::debug!("capture {alsa_name}: route {} attached", sub.route_id);
                    subscribers.push(*sub);
                }
                Command::Remove(route_id) => {
                    subscribers.retain(|s| s.route_id != route_id);
                    tracing::debug!("capture {alsa_name}: route {route_id} detached");
                }
            }
        }

        // The one blocking point in this loop — see the module docs. Waiting
        // here rather than inside `readi` is what bounds how long a stop
        // request can go unheard; everything below returns promptly, which is
        // why no error path may skip it.
        match pcm.wait(Some(DEVICE_WAIT_TIMEOUT_MS)) {
            Ok(true) => {
                // Only touch the flag on the edge. A store per period would
                // be harmless but this is the hot path of a SCHED_FIFO loop,
                // and a stall is by definition rare.
                if stalled != 0 {
                    stalled = 0;
                    w.stalled.store(false, Ordering::Relaxed);
                }
            }
            Ok(false) => {
                // Timed out: the device has not produced a period yet.
                //
                // Emphatically **not** an xrun — nothing was lost, nothing
                // was late, the device simply has not spoken. Counting it as
                // one would corrupt the only number that tells an operator
                // whether their period size is too aggressive.
                //
                // It is not counted against `consecutive_errors` either, so a
                // stalled device does not eventually kill its own route. That
                // is deliberate: the supervisor would restart the route
                // straight back into the same wedged device, and
                // `snd_pcm_open` on one of those can block far longer than
                // this loop ever does. A route that is stopped cleanly and
                // says so beats a restart loop that cannot be stopped at all.
                stalled += 1;
                if stalled == STALL_WARN_AFTER {
                    // Same threshold as the log line on purpose: what the
                    // journal calls a stall and what the UI calls a stall
                    // should be the same event.
                    w.stalled.store(true, Ordering::Relaxed);
                    tracing::warn!(
                        "capture {alsa_name}: no period for {}ms — device stalled? \
                         The route is still stoppable; nothing is being counted as an xrun.",
                        STALL_WARN_AFTER * DEVICE_WAIT_TIMEOUT_MS
                    );
                }
                pcm::ensure_capture_running(&pcm);
                continue;
            }
            Err(err) => {
                if pcm.try_recover(err, false).is_ok() {
                    w.xruns.fetch_add(1, Ordering::Relaxed);
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        bail!(
                            "capture {alsa_name}: {consecutive_errors} consecutive xruns, giving up"
                        );
                    }
                    tracing::warn!("capture {alsa_name} xrun recovered");
                    pcm::ensure_capture_running(&pcm);
                    // The ring was just reset; treat what comes out of it
                    // next like a fresh start.
                    fade.arm(RESUME_FADE_MS);
                    continue;
                }
                bail!("capture {alsa_name} wait: {err}");
            }
        }

        // The wait returned ready, which means at least `avail_min` frames
        // are queued, and `avail_min` defaults to one period — so this read
        // is satisfied from what is already there rather than blocking for
        // it. An error here is still possible (an xrun between the wait and
        // the read) and is handled the same way.
        let frames = match io.readi(&mut raw) {
            Ok(frames) => frames,
            Err(err) => {
                if pcm.try_recover(err, false).is_ok() {
                    w.xruns.fetch_add(1, Ordering::Relaxed);
                    consecutive_errors += 1;
                    if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                        bail!(
                            "capture {alsa_name}: {consecutive_errors} consecutive xruns, giving up"
                        );
                    }
                    tracing::warn!("capture {alsa_name} xrun recovered");
                    pcm::ensure_capture_running(&pcm);
                    fade.arm(RESUME_FADE_MS);
                    continue;
                }
                bail!("capture {alsa_name} read: {err}");
            }
        };
        // A short read (signal during the syscall) would otherwise send the
        // tail of the *previous* period again, so convert only what arrived.
        alsa_to_f32(format, &raw[..frames * frame_bytes], &mut device_floats);
        // Ramped on the whole frame before the windows are cut out of it. The
        // ramp is one scalar gain per frame and extraction only selects
        // channels, so this is the same arithmetic applied once for the
        // device instead of once per destination.
        fade.apply(&mut device_floats, device_channels);

        for sub in subscribers.iter_mut() {
            window::extract(
                &device_floats,
                device_channels,
                sub.channel_offset,
                sub.channels,
                &mut sub.floats,
            );
            publish_level(&sub.level_bits, peak_of(&sub.floats));

            // Non-blocking under ROC_CLOCK_SOURCE_EXTERNAL: it packetizes and
            // hands the datagram to the socket without sleeping. That is the
            // property that lets this be a loop at all — N of these still
            // leave the iteration with exactly one blocking point.
            if let Err(err) = sub.sender.write(&mut sub.floats) {
                sub.consecutive_errors += 1;
                if sub.consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    // Only this destination gives up. The device and every
                    // other route on it keep going — that is the whole point
                    // of counting these per subscriber. The route is told,
                    // through its own health, so the supervisor evicts and
                    // retries it with backoff instead of leaving it showing
                    // a healthy green over silence.
                    tracing::error!(
                        "capture {alsa_name}: route {} failed {} times in a row, dropping it: {err:#}",
                        sub.route_id,
                        sub.consecutive_errors
                    );
                    sub.health.fail(format!(
                        "sender failed {MAX_CONSECUTIVE_ERRORS} times in a row: {err:#}"
                    ));
                    sub.consecutive_errors = u32::MAX;
                    continue;
                }
                tracing::warn!("capture {alsa_name}: route {}: {err:#}", sub.route_id);
                continue;
            }
            sub.consecutive_errors = 0;
        }
        // A destination that gave up is dropped here rather than inside the
        // loop above, so the borrow ends first.
        subscribers.retain(|s| s.consecutive_errors != u32::MAX);
        consecutive_errors = 0;

        ticks += 1;
        if ticks >= metrics_every {
            ticks = 0;
            if let Some(ns) = pcm::delay_ns(&pcm, spec.rate) {
                w.buffer_ns.store(ns, Ordering::Relaxed);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundnet_protocol::Encoding;

    /// An owner with no thread behind it, for the checks that are pure
    /// bookkeeping. Anything that actually reads a card needs hardware and
    /// lives in `tests/loopback.rs`.
    fn fake_owner(device_channels: usize, roster: Vec<String>) -> CaptureOwner {
        let worker = Worker {
            stop: Arc::new(AtomicBool::new(false)),
            xruns: Arc::new(AtomicUsize::new(0)),
            buffer_ns: Arc::new(AtomicU64::new(0)),
            format: Arc::new(AtomicU8::new(UNKNOWN_FORMAT)),
            stalled: Arc::new(AtomicBool::new(false)),
        };
        CaptureOwner {
            alsa_name: "hw:CARD=UMC,DEV=0".to_string(),
            params: DeviceParams::of(&spec(48_000, 128, SampleFormat::S24Le3)),
            device_channels: Mutex::new(device_channels),
            stop: worker.stop.clone(),
            thread: Mutex::new(None),
            commands: Mutex::new(mpsc::channel().0),
            handover: Arc::new(Mutex::new(None)),
            reopening: AtomicBool::new(false),
            roster: Mutex::new(roster),
            xruns: worker.xruns.clone(),
            buffer_ns: worker.buffer_ns.clone(),
            format: worker.format.clone(),
            stalled: worker.stalled.clone(),
            last_error: Arc::new(Mutex::new(None)),
            worker,
            spec: spec(48_000, 128, SampleFormat::S24Le3),
        }
    }

    fn spec(rate: u32, period: u32, format: SampleFormat) -> StreamSpec {
        StreamSpec {
            encoding: Encoding::Pcm,
            rate,
            channels: 2,
            frames_per_period: period,
            alsa_format: format,
            target_latency_ms: 10,
            fec: true,
        }
    }

    /// A device is opened once, so everything that decides *how* it is opened
    /// has to be the same for every route on it. Everything else — where the
    /// audio goes, how much latency the far end buffers, whether FEC is on —
    /// belongs to the sender and can differ freely.
    #[test]
    fn only_the_parameters_that_open_the_device_have_to_match() {
        let base = spec(48_000, 128, SampleFormat::S24Le3);

        let mut elsewhere = base.clone();
        elsewhere.target_latency_ms = 80;
        elsewhere.fec = false;
        elsewhere.channels = 4;
        assert_eq!(
            DeviceParams::of(&base),
            DeviceParams::of(&elsewhere),
            "latency, FEC and channel count are the sender's business, not the card's"
        );

        for differing in [
            spec(96_000, 128, SampleFormat::S24Le3),
            spec(48_000, 256, SampleFormat::S24Le3),
            spec(48_000, 128, SampleFormat::S32Le),
        ] {
            assert_ne!(
                DeviceParams::of(&base),
                DeviceParams::of(&differing),
                "{differing:?} cannot share a device with {base:?}"
            );
        }
    }

    /// The message is the whole feature here. "Device or resource busy" is
    /// what this replaces, and it withheld the one fact the operator needed:
    /// which of their own routes was holding the card.
    #[test]
    fn a_rejected_route_is_told_which_route_holds_the_device() {
        let owner = CaptureOwner {
            alsa_name: "hw:CARD=UMC,DEV=0".to_string(),
            params: DeviceParams::of(&spec(48_000, 128, SampleFormat::S24Le3)),
            ..fake_owner(2, vec!["studio-to-booth".to_string()])
        };

        let clash = check_compatible(&owner, &spec(96_000, 128, SampleFormat::S24Le3), 2)
            .err()
            .expect("a different rate cannot share the device");
        let text = format!("{clash:#}");
        assert!(text.contains("studio-to-booth"), "got: {text}");
        assert!(text.contains("48000Hz"), "the held parameters: {text}");
        assert!(text.contains("96000Hz"), "the requested parameters: {text}");

        // A wider window is not a refusal — it is a reopen, which is the
        // decision the operator signed off on.
        assert!(matches!(
            check_compatible(&owner, &spec(48_000, 128, SampleFormat::S24Le3), 6),
            Ok(Fit::NeedsWider)
        ));
        assert!(matches!(
            check_compatible(&owner, &spec(48_000, 128, SampleFormat::S24Le3), 2),
            Ok(Fit::AsIs)
        ));
    }

    /// The point of the whole feature: a second route on the same device with
    /// a *different* channel window, as long as it fits inside what the card
    /// was opened for.
    #[test]
    fn a_second_route_may_take_different_channels_of_the_same_device() {
        let owner = CaptureOwner {
            alsa_name: "hw:CARD=UMC,DEV=0".to_string(),
            params: DeviceParams::of(&spec(48_000, 128, SampleFormat::S24Le3)),
            ..fake_owner(8, vec!["front-of-house".to_string()])
        };
        let same = spec(48_000, 128, SampleFormat::S24Le3);
        // Channels 7-8 of an 8-channel device: offset 6, width 8. Fits inside
        // what is already open, so nobody else is disturbed.
        assert!(matches!(check_compatible(&owner, &same, 8), Ok(Fit::AsIs)));
        // One past the edge needs the card reopened, which costs the other
        // routes a gap — so it must be distinguishable from the case above.
        assert!(matches!(
            check_compatible(&owner, &same, 9),
            Ok(Fit::NeedsWider)
        ));
    }

    /// A device can outlive one of its routes. When a destination's sender
    /// fails repeatedly the audio thread drops that subscriber and keeps
    /// reading for everybody else — and the dropped route has to be able to
    /// say so, or it sits in the UI as a healthy "ok" over silence while the
    /// device thread, the stall flag and the xrun counter all report nothing
    /// wrong.
    #[test]
    fn a_dropped_route_carries_its_own_reason() {
        let health = SubscriberHealth::default();
        assert!(!health.has_failed());
        assert_eq!(health.reason(), None);

        health.fail("sender failed 64 times in a row: no route to host".to_string());

        assert!(health.has_failed());
        let reason = health.reason().expect("a failed route must explain itself");
        assert!(reason.contains("no route to host"), "got: {reason}");
    }

    /// Unsubscribing a route from a device nobody opened has to be a no-op,
    /// not a panic: `routing` tears routes down on paths where the start may
    /// never have got as far as a device.
    #[test]
    fn unsubscribing_from_a_device_that_was_never_opened_is_harmless() {
        let registry = CaptureRegistry::default();
        registry.unsubscribe("hw:CARD=nothing,DEV=0", "some-route");
        registry.request_stop_all();
    }
}
