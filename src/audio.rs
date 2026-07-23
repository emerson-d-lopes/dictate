//! Microphone capture, downmixed and resampled to the 16 kHz mono the model expects.
//!
//! The cpal stream is owned by its own thread and never crosses one: `cpal::Stream` is `!Send` on
//! WASAPI because it holds a raw handle. Everything else talks to it over channels.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use rubato::{FftFixedIn, Resampler};

/// What the model wants.
pub const TARGET_RATE: u32 = 16_000;

/// Live input level, 0.0 to 1.0, updated from the capture callback and read by the UI so the
/// bubble reacts to real speech rather than a canned animation. Stored as `f32` bits.
static INPUT_LEVEL: AtomicU32 = AtomicU32::new(0);

/// Current smoothed input level. Zero when nothing is recording.
pub fn input_level() -> f32 {
    f32::from_bits(INPUT_LEVEL.load(Ordering::Relaxed))
}

fn set_input_level(v: f32) {
    INPUT_LEVEL.store(v.to_bits(), Ordering::Relaxed);
}

/// A recording in progress. Dropping or stopping it releases the microphone.
pub struct Recording {
    stop: Sender<()>,
    done: Receiver<Result<Captured>>,
}

struct Captured {
    samples: Vec<f32>,
    rate: u32,
}

impl Recording {
    /// Open the default input device and start capturing immediately.
    pub fn start() -> Result<Self> {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let (done_tx, done_rx) = mpsc::channel::<Result<Captured>>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();

        std::thread::Builder::new()
            .name("dictate-audio".into())
            .spawn(move || {
                let outcome = capture(&stop_rx, &ready_tx);
                let _ = done_tx.send(outcome);
            })
            .context("spawning audio thread")?;

        // Surface device errors here rather than on release, so a broken microphone is reported
        // when the user presses the key instead of after they have finished speaking.
        ready_rx
            .recv()
            .context("audio thread died during startup")??;

        Ok(Self {
            stop: stop_tx,
            done: done_rx,
        })
    }

    /// Stop capturing and return 16 kHz mono samples.
    pub fn finish(self) -> Result<Vec<f32>> {
        let _ = self.stop.send(());
        let captured = self
            .done
            .recv()
            .context("audio thread died before returning samples")??;

        if captured.samples.is_empty() {
            return Ok(Vec::new());
        }
        resample_to_target(&captured.samples, captured.rate)
    }
}

/// Runs on the audio thread for the lifetime of one recording.
fn capture(stop: &Receiver<()>, ready: &Sender<Result<()>>) -> Result<Captured> {
    let host = cpal::default_host();
    let device = match host.default_input_device() {
        Some(d) => d,
        None => {
            let _ = ready.send(Err(anyhow!("no input device")));
            return Err(anyhow!("no input device"));
        }
    };

    let config = match device.default_input_config() {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(anyhow!("no default input config: {e}")));
            return Err(anyhow!("no default input config: {e}"));
        }
    };

    let rate = config.sample_rate();
    let channels = config.channels() as usize;
    let buffer = Arc::new(Mutex::new(Vec::<f32>::new()));

    set_input_level(0.0);
    let sink = Arc::clone(&buffer);
    // Smoothed level with a fast attack and slow release, so the bubble jumps to speech and eases
    // back down rather than flickering per callback.
    let mut level = 0.0f32;
    let stream = device.build_input_stream(
        config.into(),
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            let mut out = match sink.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            // Average the channels rather than taking the first: some headsets put the usable
            // signal on the second one.
            let mut peak = 0.0f32;
            for frame in data.chunks(channels) {
                let sum: f32 = frame.iter().sum();
                let mono = sum / channels as f32;
                peak = peak.max(mono.abs());
                out.push(mono);
            }
            level = if peak > level {
                peak
            } else {
                level * 0.80 + peak * 0.20
            };
            set_input_level(level);
        },
        |err| eprintln!("audio stream error: {err}"),
        None,
    );

    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(anyhow!("could not open microphone: {e}")));
            return Err(anyhow!("could not open microphone: {e}"));
        }
    };

    if let Err(e) = stream.play() {
        let _ = ready.send(Err(anyhow!("could not start microphone: {e}")));
        return Err(anyhow!("could not start microphone: {e}"));
    }

    let _ = ready.send(Ok(()));

    // Block until the hotkey is released. The stream keeps filling the buffer from its own
    // callback thread meanwhile.
    let _ = stop.recv();
    drop(stream);
    set_input_level(0.0);

    let samples = buffer.lock().map_err(|_| anyhow!("audio buffer poisoned"))?;
    Ok(Captured {
        samples: samples.clone(),
        rate,
    })
}

/// Resample to [`TARGET_RATE`].
///
/// Never decimate by picking every Nth sample: without the low-pass, content above 8 kHz folds
/// straight into the speech band and measurably degrades recognition.
fn resample_to_target(samples: &[f32], rate: u32) -> Result<Vec<f32>> {
    if rate == TARGET_RATE {
        return Ok(samples.to_vec());
    }

    let chunk = 1024;
    let mut resampler = FftFixedIn::<f32>::new(rate as usize, TARGET_RATE as usize, chunk, 2, 1)
        .context("building resampler")?;

    let mut out = Vec::with_capacity(samples.len() * TARGET_RATE as usize / rate as usize + chunk);
    let mut pos = 0;
    while pos + chunk <= samples.len() {
        let block = vec![samples[pos..pos + chunk].to_vec()];
        let resampled = resampler.process(&block, None).context("resampling")?;
        out.extend_from_slice(&resampled[0]);
        pos += chunk;
    }

    // Pad the final partial block with silence so the tail is not dropped. Trailing words matter.
    if pos < samples.len() {
        let mut last = samples[pos..].to_vec();
        last.resize(chunk, 0.0);
        let resampled = resampler.process(&vec![last], None).context("resampling")?;
        out.extend_from_slice(&resampled[0]);
    }

    Ok(out)
}

/// Peak amplitude, for rejecting silence before paying for inference.
pub fn peak(samples: &[f32]) -> f32 {
    samples.iter().fold(0.0f32, |m, s| m.max(s.abs()))
}

/// Shared between the persistent capture callback and the control methods.
struct PrerollState {
    /// Rolling buffer of the most recent audio at the device's native rate. Bounded to the
    /// pre-roll length while idle, so it always holds "the last N ms".
    ring: VecDeque<f32>,
    ring_cap: usize,
    /// While recording, samples accumulate here (seeded with the ring at start).
    active: Option<Vec<f32>>,
    channels: usize,
}

/// A microphone kept open continuously, so the moment a recording starts the last `preroll_ms`
/// are already in hand and the first word is never clipped.
///
/// The tradeoff is deliberate and documented in the config: the device stays open for the life of
/// the app, which keeps the Windows microphone indicator lit.
pub struct PrerollMic {
    state: Arc<Mutex<PrerollState>>,
    rate: u32,
    // The capture thread owns the cpal stream (which is !Send) and parks holding it alive.
    _thread: std::thread::JoinHandle<()>,
}

impl PrerollMic {
    /// Open the default input device and start the rolling buffer.
    pub fn new(preroll_ms: u64) -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(Arc<Mutex<PrerollState>>, u32)>>();

        let thread = std::thread::Builder::new()
            .name("dictate-preroll-mic".into())
            .spawn(move || run_preroll(preroll_ms, &ready_tx))
            .context("spawning pre-roll mic thread")?;

        let (state, rate) = ready_rx
            .recv()
            .context("pre-roll mic thread died during startup")??;

        Ok(Self {
            state,
            rate,
            _thread: thread,
        })
    }

    /// Begin a recording, seeded with the buffered pre-roll.
    pub fn begin(&self) {
        if let Ok(mut s) = self.state.lock() {
            let preroll: Vec<f32> = s.ring.iter().copied().collect();
            s.active = Some(preroll);
        }
    }

    /// End the recording and return 16 kHz mono samples (pre-roll included).
    pub fn end(&self) -> Result<Vec<f32>> {
        let native = {
            let mut s = self.state.lock().map_err(|_| anyhow!("mic state poisoned"))?;
            s.active.take().unwrap_or_default()
        };
        if native.is_empty() {
            return Ok(Vec::new());
        }
        resample_to_target(&native, self.rate)
    }
}

/// Runs on the pre-roll mic thread for the life of the app.
fn run_preroll(
    preroll_ms: u64,
    ready: &Sender<Result<(Arc<Mutex<PrerollState>>, u32)>>,
) {
    let host = cpal::default_host();
    let device = match host.default_input_device() {
        Some(d) => d,
        None => {
            let _ = ready.send(Err(anyhow!("no input device")));
            return;
        }
    };
    let config = match device.default_input_config() {
        Ok(c) => c,
        Err(e) => {
            let _ = ready.send(Err(anyhow!("no default input config: {e}")));
            return;
        }
    };

    let rate = config.sample_rate();
    let channels = config.channels() as usize;
    // Ring holds pre-roll worth of mono samples at the native rate.
    let ring_cap = (rate as usize * preroll_ms as usize / 1000).max(1);

    let state = Arc::new(Mutex::new(PrerollState {
        ring: VecDeque::with_capacity(ring_cap + 1),
        ring_cap,
        active: None,
        channels,
    }));

    let sink = Arc::clone(&state);
    let mut level = 0.0f32;
    let stream = device.build_input_stream(
        config.into(),
        move |data: &[f32], _: &cpal::InputCallbackInfo| {
            let mut s = match sink.lock() {
                Ok(g) => g,
                Err(_) => return,
            };
            let channels = s.channels;
            let mut peak = 0.0f32;
            for frame in data.chunks(channels) {
                let sum: f32 = frame.iter().sum();
                let mono = sum / channels as f32;
                peak = peak.max(mono.abs());

                if let Some(active) = s.active.as_mut() {
                    active.push(mono);
                } else {
                    // Idle: keep the ring bounded to the pre-roll window.
                    if s.ring.len() == s.ring_cap {
                        s.ring.pop_front();
                    }
                    s.ring.push_back(mono);
                }
            }
            level = if peak > level {
                peak
            } else {
                level * 0.80 + peak * 0.20
            };
            set_input_level(level);
        },
        |err| eprintln!("preroll audio error: {err}"),
        None,
    );

    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            let _ = ready.send(Err(anyhow!("could not open microphone: {e}")));
            return;
        }
    };
    if let Err(e) = stream.play() {
        let _ = ready.send(Err(anyhow!("could not start microphone: {e}")));
        return;
    }

    let _ = ready.send(Ok((Arc::clone(&state), rate)));

    // Hold the stream alive for the life of the app. Process exit reclaims it.
    loop {
        std::thread::park();
    }
}
