//! Sound out: the SPU's 44.1 kHz stereo to the default output device.
//!
//! The emulator thread stays the clock (it paces frames by the emulated time), so the
//! game runs the same with or without a sound device. Each frame's samples are
//! resampled to the device's own rate and pushed into a lock-free queue the
//! device callback drains. The two clocks drift apart slowly, so the resample
//! ratio is nudged by up to ±0.5 % to hold the queue near [`TARGET_MS`]:
//! too little queued and it makes a few more samples, too much and a few
//! fewer. That is far below what an ear notices as pitch.
//!
//! The device's sample rate is never changed (that would change it for every
//! app on the machine); the stream takes whatever the device runs at.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

pub use lt_emu_protocol::AudioStats;

/// The SPU's output rate.
pub const SPU_RATE: u32 = 44_100;
/// How much sound to keep queued: enough to ride out a late frame, little enough to stay in sync with the picture.
pub const TARGET_MS: f64 = 60.0;
/// The queue's size.
const CAPACITY_MS: f64 = 250.0;
/// The largest nudge to the resample ratio.
const MAX_ADJUST: f64 = 0.005;

/// Volume and mute, shared with the device callback (and changed from anywhere).
#[derive(Debug)]
pub struct Control {
    volume: AtomicU32,
    muted: AtomicBool,
}

impl Control {
    pub fn new(volume: f32, muted: bool) -> Arc<Self> {
        Arc::new(Self { volume: AtomicU32::new(volume.clamp(0.0, 1.0).to_bits()), muted: AtomicBool::new(muted) })
    }

    pub fn set_volume(&self, v: f32) {
        self.volume.store(v.clamp(0.0, 1.0).to_bits(), Ordering::Relaxed);
    }

    pub fn set_muted(&self, m: bool) {
        self.muted.store(m, Ordering::Relaxed);
    }

    pub fn volume(&self) -> f32 {
        f32::from_bits(self.volume.load(Ordering::Relaxed))
    }

    pub fn muted(&self) -> bool {
        self.muted.load(Ordering::Relaxed)
    }

    /// The gain the callback applies: a perceptual curve, so the slider's middle sounds like the middle.
    fn gain(&self) -> f32 {
        if self.muted() {
            0.0
        } else {
            let v = self.volume();
            v * v
        }
    }
}

/// What the callback reports back.
#[derive(Debug, Default)]
struct Counters {
    /// Device frames written with no sound queued, while playing.
    underruns: AtomicU64,
    /// Device frames written.
    played: AtomicU64,
    /// Set by the stream's error callback (device unplugged, default changed away, …).
    failed: AtomicBool,
    /// The emulator stopped feeding (paused): silence that isn't an underrun, and drop what's queued.
    paused: AtomicBool,
    /// Drop everything queued at the next callback.
    flush: AtomicBool,
    /// Enough sound is queued to start (after opening or a flush): until then the callback plays silence.
    started: AtomicBool,
    /// The device has taken sound since the last start. Until it does (a Bluetooth device can take a
    /// fifth of a second to start pulling), the queue is held at the target instead of growing.
    consuming: AtomicBool,
}

/// An open output stream. Not `Send` on every platform (CoreAudio streams aren't): create and keep it on one thread.
pub struct AudioOut {
    stream: cpal::Stream,
    prod: HeapProd<f32>,
    counters: Arc<Counters>,
    device: String,
    rate: u32,
    channels: u16,
    resampler: Resampler,
    scratch: Vec<f32>,
    overruns: u64,
    adjust: f64,
    fed: u64,
}

/// The default output device's name, if there is one (to notice the user switching devices).
pub fn default_device_name() -> Option<String> {
    cpal::default_host().default_output_device().and_then(|d| d.name().ok())
}

impl AudioOut {
    /// Opens the default output device at its own rate and format, playing silence until sound arrives.
    pub fn open(control: Arc<Control>) -> Result<Self, String> {
        let device = cpal::default_host().default_output_device().ok_or("no sound output device")?;
        let name = device.name().unwrap_or_else(|_| "output".into());
        let supported = device.default_output_config().map_err(|e| format!("{name}: {e}"))?;
        let format = supported.sample_format();
        let config: cpal::StreamConfig = supported.into();
        let (rate, channels) = (config.sample_rate.0, config.channels);
        let capacity = (rate as f64 * CAPACITY_MS / 1000.0) as usize * 2;
        let (prod, cons) = HeapRb::<f32>::new(capacity).split();
        let counters = Arc::new(Counters::default());
        let stream = match format {
            cpal::SampleFormat::F32 => build::<f32>(&device, &config, cons, control, counters.clone()),
            cpal::SampleFormat::I16 => build::<i16>(&device, &config, cons, control, counters.clone()),
            cpal::SampleFormat::U16 => build::<u16>(&device, &config, cons, control, counters.clone()),
            cpal::SampleFormat::I32 => build::<i32>(&device, &config, cons, control, counters.clone()),
            other => return Err(format!("{name}: sample format {other} isn't supported")),
        }
        .map_err(|e| format!("{name}: {e}"))?;
        stream.play().map_err(|e| format!("{name}: {e}"))?;
        Ok(Self {
            stream,
            prod,
            counters,
            device: name,
            rate,
            channels,
            resampler: Resampler::default(),
            scratch: Vec::with_capacity(4096),
            overruns: 0,
            adjust: 0.0,
            fed: 0,
        })
    }

    pub fn device(&self) -> &str {
        &self.device
    }

    /// The stream reported an error (the device went away); reopen.
    pub fn failed(&self) -> bool {
        self.counters.failed.load(Ordering::Relaxed)
    }

    fn queued_frames(&self) -> usize {
        self.prod.occupied_len() / 2
    }

    /// Queues one frame's SPU samples (44.1 kHz stereo).
    pub fn push(&mut self, samples: &[(i16, i16)]) {
        if samples.is_empty() {
            return;
        }
        self.fed += samples.len() as u64;
        let target = self.rate as f64 * TARGET_MS / 1000.0;
        // Before the device pulls, hold at the target; after, never past twice it (a stall upstream
        // must not turn into lasting delay): drop this frame's sound instead.
        let cap = if self.counters.consuming.load(Ordering::Relaxed) { 2.0 * target } else { target };
        if self.queued_frames() as f64 >= cap {
            self.overruns += samples.len() as u64;
            return;
        }
        let error = ((target - self.queued_frames() as f64) / target).clamp(-1.0, 1.0);
        self.adjust = MAX_ADJUST * error;
        let step = SPU_RATE as f64 / (self.rate as f64 * (1.0 + self.adjust));
        self.scratch.clear();
        self.resampler.process(samples, step, &mut self.scratch);
        let pushed = self.prod.push_slice(&self.scratch);
        self.overruns += (self.scratch.len() - pushed) as u64 / 2;
        // Play once the target is queued, so the first callbacks don't run dry.
        if self.queued_frames() as f64 >= target {
            self.counters.started.store(true, Ordering::Relaxed);
        }
    }

    /// Stops or resumes the sound with the game. Pausing drops what's queued, so a resume doesn't replay it.
    /// True when it changed anything (starting or stopping a device can take a tenth of a second).
    pub fn set_paused(&mut self, paused: bool) -> bool {
        let changed = paused != self.counters.paused.load(Ordering::Relaxed);
        if changed {
            self.counters.paused.store(paused, Ordering::Relaxed);
            self.flush();
            if paused {
                let _ = self.stream.pause();
            } else {
                let _ = self.stream.play();
            }
        }
        changed
    }

    /// Drops what's queued (after a reset or a snapshot load).
    pub fn flush(&mut self) {
        self.counters.flush.store(true, Ordering::Relaxed);
        self.counters.started.store(false, Ordering::Relaxed);
        self.counters.consuming.store(false, Ordering::Relaxed);
        self.resampler = Resampler::default();
    }

    pub fn stats(&self) -> AudioStats {
        AudioStats {
            device: self.device.clone(),
            rate: self.rate,
            channels: self.channels,
            queued_ms: self.queued_frames() as f64 * 1000.0 / self.rate as f64,
            underruns: self.counters.underruns.load(Ordering::Relaxed),
            overruns: self.overruns,
            adjust_ppm: self.adjust * 1e6,
            fed: self.fed,
            played: self.counters.played.load(Ordering::Relaxed),
        }
    }
}

fn build<T>(device: &cpal::Device, config: &cpal::StreamConfig, mut cons: HeapCons<f32>, control: Arc<Control>, counters: Arc<Counters>) -> Result<cpal::Stream, cpal::BuildStreamError>
where
    T: cpal::SizedSample + cpal::FromSample<f32>,
{
    let channels = config.channels as usize;
    let errors = counters.clone();
    let mut buf = [0f32; 2048];
    device.build_output_stream(
        config,
        move |data: &mut [T], _: &cpal::OutputCallbackInfo| {
            if counters.flush.swap(false, Ordering::Relaxed) {
                cons.clear();
            }
            let paused = counters.paused.load(Ordering::Relaxed);
            let gain = control.gain();
            let frames = data.len() / channels;
            let mut done = 0;
            let mut short = 0u64;
            while done < frames {
                let n = (frames - done).min(buf.len() / 2);
                let playing = !paused && counters.started.load(Ordering::Relaxed);
                let got = if playing { cons.pop_slice(&mut buf[..n * 2]) / 2 } else { 0 };
                if got > 0 {
                    counters.consuming.store(true, Ordering::Relaxed);
                }
                short += (n - got) as u64;
                for i in 0..n {
                    let (l, r) = if i < got { (buf[i * 2] * gain, buf[i * 2 + 1] * gain) } else { (0.0, 0.0) };
                    let out = &mut data[(done + i) * channels..(done + i + 1) * channels];
                    match channels {
                        1 => out[0] = T::from_sample((l + r) * 0.5),
                        _ => {
                            out[0] = T::from_sample(l);
                            out[1] = T::from_sample(r);
                            for extra in &mut out[2..] {
                                *extra = T::from_sample(0.0);
                            }
                        }
                    }
                }
                done += n;
            }
            counters.played.fetch_add(frames as u64, Ordering::Relaxed);
            if !paused && counters.started.load(Ordering::Relaxed) {
                counters.underruns.fetch_add(short, Ordering::Relaxed);
            }
        },
        move |_err| errors.failed.store(true, Ordering::Relaxed),
        None,
    )
}

/// Linear resampling of stereo i16 to interleaved f32, keeping its place across calls.
#[derive(Debug, Clone, Copy)]
struct Resampler {
    /// Position between `prev` and the next input sample, 0..1.
    pos: f64,
    prev: (f32, f32),
}

impl Default for Resampler {
    fn default() -> Self {
        Self { pos: 0.0, prev: (0.0, 0.0) }
    }
}

impl Resampler {
    /// `step` = input samples per output sample.
    fn process(&mut self, input: &[(i16, i16)], step: f64, out: &mut Vec<f32>) {
        const SCALE: f32 = 1.0 / 32768.0;
        for &(l, r) in input {
            let cur = (l as f32 * SCALE, r as f32 * SCALE);
            while self.pos < 1.0 {
                let t = self.pos as f32;
                out.push(self.prev.0 + (cur.0 - self.prev.0) * t);
                out.push(self.prev.1 + (cur.1 - self.prev.1) * t);
                self.pos += step;
            }
            self.pos -= 1.0;
            self.prev = cur;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resampling_keeps_the_rate_and_the_waveform() {
        // One second of a 441 Hz square wave at 44.1 kHz, to 48 kHz in frame-sized pieces.
        let input: Vec<(i16, i16)> = (0..44_100).map(|i| if (i / 50) % 2 == 0 { (16_000, -16_000) } else { (-16_000, 16_000) }).collect();
        let mut r = Resampler::default();
        let mut out = Vec::new();
        for piece in input.chunks(736) {
            r.process(piece, 44_100.0 / 48_000.0, &mut out);
        }
        let frames = out.len() / 2;
        assert!((47_990..=48_010).contains(&frames), "{frames} frames");
        assert!(out.iter().all(|s| s.abs() <= 16_000.0 / 32_768.0 + 1e-6), "no overshoot");
        // Left and right stay opposite: channels aren't swapped or smeared.
        assert!(out.chunks(2).all(|f| (f[0] + f[1]).abs() < 1e-6));
    }

    #[test]
    fn a_nudged_step_makes_more_or_fewer_samples() {
        let input = vec![(1000i16, 1000i16); 44_100];
        let count = |adjust: f64| {
            let mut r = Resampler::default();
            let mut out = Vec::new();
            r.process(&input, 44_100.0 / (48_000.0 * (1.0 + adjust)), &mut out);
            out.len() / 2
        };
        let (less, base, more) = (count(-MAX_ADJUST), count(0.0), count(MAX_ADJUST));
        assert!(less < base && base < more);
        assert!((more - base) as f64 / base as f64 <= MAX_ADJUST + 1e-4);
    }

    #[test]
    fn the_volume_curve_and_mute() {
        let c = Control::new(0.5, false);
        assert!((c.gain() - 0.25).abs() < 1e-6);
        c.set_muted(true);
        assert_eq!(c.gain(), 0.0);
        c.set_volume(7.0);
        assert_eq!(c.volume(), 1.0);
    }
}
