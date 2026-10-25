//! The emulator thread: requests between frames, frames in real time (paced by the emulated clock,
//! with sound), pictures and stats out.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

use lt_emu_protocol::{Answer, Envelope, Event, FrameHeader, RamOp, Request, Stats, PROTOCOL};

use crate::audio::{self, AudioOut, Control};
use crate::disc::{Image, Overlay};
use crate::machine::{Machine, CORE, CPU_HZ};

/// A frame's wait is the emulated time it covered, bounded so a restore or a stall can't make the
/// loop sleep long or spin.
const FRAME_MIN: Duration = Duration::from_millis(8);
const FRAME_MAX: Duration = Duration::from_millis(34);
/// Pictures queued for the pipe before new ones are dropped (the application isn't reading).
const PICTURES_QUEUED: usize = 2;

/// What goes to stdout, in order.
pub enum Outgoing {
    Event(Event),
    Picture(FrameHeader, Vec<u8>),
}

/// The pipe out: events always go; pictures are dropped while [`PICTURES_QUEUED`] wait.
#[derive(Clone)]
pub struct Out {
    tx: Sender<Outgoing>,
    queued: Arc<AtomicUsize>,
    sent: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
}

impl Out {
    pub fn new(tx: Sender<Outgoing>, queued: Arc<AtomicUsize>) -> Self {
        Self { tx, queued, sent: Arc::default(), dropped: Arc::default() }
    }

    pub fn event(&self, e: Event) {
        let _ = self.tx.send(Outgoing::Event(e));
    }

    fn reply(&self, id: u32, result: Result<Answer, String>) {
        if id != 0 {
            self.event(Event::Reply { id, result });
        }
    }

    fn picture(&self, header: FrameHeader, rgba: Vec<u8>) {
        if self.queued.load(Ordering::Relaxed) >= PICTURES_QUEUED {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        self.queued.fetch_add(1, Ordering::Relaxed);
        self.sent.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(Outgoing::Picture(header, rgba));
    }
}

/// The sound device, opened on the emulator thread (CoreAudio streams can't move between threads)
/// and reopened when it fails or the default output changes.
struct Sound {
    out: Option<AudioOut>,
    control: Arc<Control>,
    error: Option<String>,
    checked: Instant,
}

impl Sound {
    fn open(control: Arc<Control>) -> Self {
        let mut s = Self { out: None, control, error: None, checked: Instant::now() };
        s.reopen();
        s
    }

    fn reopen(&mut self) {
        self.out = None;
        match AudioOut::open(self.control.clone()) {
            Ok(o) => {
                self.out = Some(o);
                self.error = None;
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Every 2 s: reopen after a stream error, or when the default output device changed. True
    /// when it reopened (which can take a fifth of a second).
    fn watch(&mut self) -> bool {
        if self.checked.elapsed() < Duration::from_secs(2) {
            return false;
        }
        self.checked = Instant::now();
        let now = audio::default_device_name();
        let stale = match &self.out {
            Some(o) => o.failed() || now.as_deref() != Some(o.device()),
            None => now.is_some(),
        };
        if stale {
            self.reopen();
        }
        stale
    }

    fn push(&mut self, samples: &[(i16, i16)]) {
        if let Some(o) = self.out.as_mut() {
            o.push(samples);
        }
    }

    fn set_paused(&mut self, paused: bool) -> bool {
        self.out.as_mut().is_some_and(|o| o.set_paused(paused))
    }

    fn flush(&mut self) {
        if let Some(o) = self.out.as_mut() {
            o.flush();
        }
    }
}

/// The stats over the window since `start`.
#[derive(Default)]
struct Window {
    start: Option<Instant>,
    ticks: u64,
    frames: u64,
    emu: f64,
    copy: f64,
    emu_max: f64,
    emulated: f64,
}

struct Runner {
    out: Out,
    machine: Option<Machine>,
    running: bool,
    halted: bool,
    pad: u16,
    /// Buttons held until a frame, and the request to answer then.
    press: Option<(u16, u64, u32)>,
    pictures_every: u32,
    control: Arc<Control>,
    sound: Option<Sound>,
    window: Window,
    stats: Stats,
    stats_sent: Instant,
    next: Instant,
}

pub fn run(rx: Receiver<Envelope>, out: Out) {
    crate::thread::interactive();
    let mut r = Runner {
        out,
        machine: None,
        running: false,
        halted: false,
        pad: 0,
        press: None,
        pictures_every: 0,
        control: Control::new(0.8, false),
        sound: None,
        window: Window::default(),
        stats: Stats::default(),
        stats_sent: Instant::now(),
        next: Instant::now(),
    };
    loop {
        // Requests are served the moment they come, while the loop waits for the next frame's
        // time (that wait is between two frames); when not running, it just waits for them.
        let active = r.running && !r.halted && r.machine.is_some();
        let wait = if active { r.next.saturating_duration_since(Instant::now()) } else { Duration::from_millis(50) };
        match rx.recv_timeout(wait) {
            Ok(env) => {
                if !r.handle(env) {
                    return;
                }
                while let Ok(env) = rx.try_recv() {
                    if !r.handle(env) {
                        return;
                    }
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if r.running && !r.halted && r.machine.is_some() {
            if Instant::now() >= r.next {
                r.frame();
            }
        } else {
            if let Some(s) = r.sound.as_mut() {
                s.set_paused(true);
            }
            r.next = Instant::now();
        }
        if r.stats_sent.elapsed() >= Duration::from_secs(1) {
            r.send_stats();
        }
    }
}

impl Runner {
    /// One request. False: shut down.
    fn handle(&mut self, Envelope { id, request }: Envelope) -> bool {
        let result = match request {
            Request::Hello { protocol } => {
                if protocol != PROTOCOL {
                    Err(format!("this host speaks protocol {PROTOCOL}, not {protocol}"))
                } else {
                    Ok(Answer::Hello { protocol: PROTOCOL, core: CORE.into(), host: format!("lt-emu-host {}", env!("CARGO_PKG_VERSION")) })
                }
            }
            Request::Open { image, sectors, patches, card } => self.open(&image, sectors, patches, card).map(|()| Answer::Ok),
            Request::Patch { set, clear } => self.with(|m| m.overlay().patch(set.into_iter().map(|s| (s.lba, s.raw)).collect(), &clear).map(|()| Answer::Ok)),
            Request::Run => {
                self.running = true;
                self.next = Instant::now();
                Ok(Answer::Ok)
            }
            Request::Pause => {
                self.running = false;
                Ok(Answer::Ok)
            }
            Request::Reset => {
                let r = self.with(|m| m.reset().map(|()| Answer::Ok));
                if r.is_ok() {
                    self.halted = false;
                    self.flush_sound();
                }
                r
            }
            Request::Pad { buttons } => {
                self.pad = buttons;
                Ok(Answer::Ok)
            }
            Request::Press { buttons, frames } => match self.machine.as_ref() {
                None => Err(not_open()),
                Some(m) if frames == 0 => Ok(Answer::Frame { frame: m.frame_count(), emu_ms: 0.0, audio_peak: 0, width: 0, height: 0 }),
                Some(m) => {
                    // An earlier press still held is let go now.
                    if let Some((_, _, old)) = self.press.take() {
                        self.out.reply(old, Ok(Answer::Frame { frame: m.frame_count(), emu_ms: 0.0, audio_peak: 0, width: 0, height: 0 }));
                    }
                    self.press = Some((buttons, m.frame_count() + u64::from(frames), id));
                    return true;
                }
            },
            Request::Advance { pads, show_last } => self.advance(&pads, show_last),
            Request::Batch { ops } => self.with(|m| Ok(batch(m, &ops))),
            Request::SaveState => self.with(|m| m.save_state().map(|state| Answer::State { frame: m.frame_count(), state })),
            Request::LoadState { frame, state } => {
                let r = self.with(|m| m.load_state(frame, &state).map(|()| Answer::Ok));
                if r.is_ok() {
                    self.halted = false;
                    self.flush_sound();
                }
                r
            }
            Request::Card => self.with(|m| Ok(Answer::Card(m.card()))),
            Request::InsertCard { card } => self.with(|m| {
                m.insert_card(card);
                Ok(Answer::Ok)
            }),
            Request::Screenshot => self.with(|m| Ok(Answer::Picture(m.picture()))),
            Request::Frames { every } => {
                self.pictures_every = every;
                Ok(Answer::Ok)
            }
            Request::Audio { volume, muted } => {
                self.control.set_volume(volume);
                self.control.set_muted(muted);
                Ok(Answer::Ok)
            }
            Request::Diag => self.with(|m| Ok(Answer::Diag(m.diag()))),
            Request::Shutdown => {
                let card = self.machine.as_mut().map(Machine::card).unwrap_or_default();
                self.out.reply(id, Ok(Answer::Card(card)));
                return false;
            }
        };
        self.out.reply(id, result);
        true
    }

    fn with(&mut self, f: impl FnOnce(&mut Machine) -> Result<Answer, String>) -> Result<Answer, String> {
        self.machine.as_mut().ok_or_else(not_open).and_then(f)
    }

    fn open(&mut self, image: &str, sectors: u32, patches: Vec<lt_emu_protocol::Sector>, card: Vec<u8>) -> Result<(), String> {
        let overlay = Arc::new(Overlay::new(Image::open(std::path::Path::new(image))?, sectors));
        overlay.patch(patches.into_iter().map(|s| (s.lba, s.raw)).collect(), &[])?;
        self.machine = Some(Machine::open(overlay, card)?);
        self.running = false;
        self.halted = false;
        self.press = None;
        self.window = Window::default();
        if self.sound.is_none() {
            self.sound = Some(Sound::open(self.control.clone()));
        }
        self.flush_sound();
        Ok(())
    }

    fn flush_sound(&mut self) {
        if let Some(s) = self.sound.as_mut() {
            s.flush();
        }
    }

    /// Frames as fast as possible with a pad each: no pacing, no sound out (its loudest sample is reported).
    fn advance(&mut self, pads: &[u16], show_last: bool) -> Result<Answer, String> {
        let m = self.machine.as_mut().ok_or_else(not_open)?;
        let t = Instant::now();
        let mut peak = 0u16;
        for &pad in pads {
            let r = m.run_frame(pad, true);
            for (l, r) in m.drain_audio() {
                peak = peak.max(l.unsigned_abs()).max(r.unsigned_abs());
            }
            if let Err(e) = r {
                self.halted = true;
                self.out.event(Event::Halted { error: e.clone() });
                return Err(e);
            }
            if m.frame_count() % 30 == 0 {
                if let Some(card) = m.take_card_write() {
                    self.out.event(Event::Card(card));
                }
            }
        }
        let frame = m.frame_count();
        let emu_ms = t.elapsed().as_secs_f64() * 1000.0;
        let (width, height) = m.display_size();
        if show_last && self.pictures_every > 0 {
            let p = m.picture();
            let header = FrameHeader { frame, emulated: m.seconds(), emu_ms: 0.0, width: p.width as u16, height: p.height as u16, sent_at: wall_ms() };
            self.out.picture(header, p.rgba);
        }
        Ok(Answer::Frame { frame, emu_ms, audio_peak: peak, width, height })
    }

    /// One frame in real time.
    fn frame(&mut self) {
        let sound = self.sound.get_or_insert_with(|| Sound::open(self.control.clone()));
        if sound.watch() || sound.set_paused(false) {
            // The device took a moment to (re)start: don't race to catch up and pile that time up as delayed sound.
            self.next = Instant::now();
        }
        let m = self.machine.as_mut().expect("open");
        let t0 = Instant::now();
        let c0 = m.cycles();
        let pressed = match self.press {
            Some((bits, until, _)) if m.frame_count() < until => bits,
            _ => 0,
        };
        if let Err(e) = m.run_frame(self.pad | pressed, true) {
            self.halted = true;
            self.out.event(Event::Halted { error: e });
            return;
        }
        let emulated = Duration::from_secs_f64(m.cycles().saturating_sub(c0) as f64 / CPU_HZ);
        let samples = m.drain_audio();
        let emu_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let frame = m.frame_count();
        if let Some((_, until, id)) = self.press {
            if frame >= until {
                self.press = None;
                self.out.reply(id, Ok(Answer::Frame { frame, emu_ms: 0.0, audio_peak: 0, width: 0, height: 0 }));
            }
        }
        // The game wrote its memory card: hand it over within half a second.
        if frame % 30 == 0 {
            if let Some(card) = m.take_card_write() {
                self.out.event(Event::Card(card));
            }
        }
        let mut copy_ms = 0.0;
        if self.pictures_every > 0 && frame % u64::from(self.pictures_every) == 0 {
            let t1 = Instant::now();
            let p = m.picture();
            copy_ms = t1.elapsed().as_secs_f64() * 1000.0;
            let header = FrameHeader { frame, emulated: m.seconds(), emu_ms: emu_ms as f32, width: p.width as u16, height: p.height as u16, sent_at: wall_ms() };
            self.stats.width = p.width;
            self.stats.height = p.height;
            self.out.picture(header, p.rgba);
        }
        sound.push(&samples);
        self.stats.frames = frame;
        // A window runs from one frame's start to the start of the frame that closes it, so its span includes every sleep.
        let start = *self.window.start.get_or_insert(t0);
        if t0.duration_since(start).as_secs_f64() >= 1.0 {
            let span = t0.duration_since(start).as_secs_f64();
            let w = std::mem::take(&mut self.window);
            let frames = w.frames.max(1) as f64;
            self.stats.fps = w.frames as f64 / span;
            self.stats.emu_ms = w.emu / frames;
            self.stats.copy_ms = w.copy / w.ticks.max(1) as f64;
            self.stats.emu_ms_max = w.emu_max;
            self.stats.field_hz = if w.emulated > 0.0 { w.frames as f64 / w.emulated } else { 0.0 };
            self.window.start = Some(t0);
        }
        let w = &mut self.window;
        w.ticks += 1;
        w.frames += 1;
        w.emu += emu_ms;
        w.copy += copy_ms;
        w.emu_max = w.emu_max.max(emu_ms);
        w.emulated += emulated.as_secs_f64();
        // The next frame is due when the emulated time this one covered has passed: sound plays at
        // its own speed, and the audio queue only absorbs the clocks' small drift.
        self.next += emulated.clamp(FRAME_MIN, FRAME_MAX);
        let now = Instant::now();
        if now > self.next && now - self.next > Duration::from_millis(250) {
            self.next = now;
        }
    }

    fn send_stats(&mut self) {
        self.stats_sent = Instant::now();
        let s = &mut self.stats;
        s.open = self.machine.is_some();
        s.running = self.running && !self.halted;
        if !s.running {
            s.fps = 0.0;
        }
        if let Some(m) = self.machine.as_ref() {
            let o = m.overlay();
            (s.reads, s.patched_reads) = o.read_counts();
            s.sectors = o.sectors();
            s.patched = o.patched() as u32;
            s.frames = m.frame_count();
        }
        s.audio = self.sound.as_ref().and_then(|x| x.out.as_ref()).map(AudioOut::stats);
        s.audio_error = self.sound.as_ref().and_then(|x| x.error.clone());
        s.pictures_sent = self.out.sent.load(Ordering::Relaxed);
        s.pictures_dropped = self.out.dropped.load(Ordering::Relaxed);
        self.out.event(Event::Stats(s.clone()));
    }
}

/// A [`Request::Batch`]: guards, then reads (as RAM is before the writes), then writes if every guard held.
fn batch(m: &mut Machine, ops: &[RamOp]) -> Answer {
    let applied = ops.iter().all(|op| match op {
        RamOp::Guard { addr, bytes } => m.read_ram(*addr, bytes.len()) == *bytes,
        _ => true,
    });
    let reads = ops
        .iter()
        .filter_map(|op| match op {
            RamOp::Read { addr, len } => Some(m.read_ram(*addr, *len as usize)),
            _ => None,
        })
        .collect();
    if applied {
        for op in ops {
            if let RamOp::Write { addr, bytes } = op {
                m.write_ram(*addr, bytes);
            }
        }
    }
    Answer::Batch { reads, applied, frame: m.frame_count() }
}

fn not_open() -> String {
    "no disc is open".into()
}

fn wall_ms() -> f64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64() * 1000.0)
}
