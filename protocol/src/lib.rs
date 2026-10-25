//! The pipe protocol between an application and `lt-emu-host`, a PS1 emulator host program.
//!
//! The application starts the host and talks to it over the host's stdin (requests) and stdout
//! (replies, events and pictures). Everything that crosses is plain data: paths, sector bytes, RAM
//! addresses and bytes, pad bits, pictures, numbers. No emulator type does, so either side can be
//! replaced without the other.
//!
//! Each message is a little-endian `u32` length, then that many bytes: a kind byte and a payload.
//! Kind [`KIND_MESSAGE`] carries a [postcard](https://docs.rs/postcard) encoding of [`Envelope`]
//! (to the host) or [`Event`] (from it); kind [`KIND_FRAME`] is a picture, a [`FrameHeader`] then
//! RGBA8 rows, kept out of postcard so 60 of them a second cost one copy each way.
//!
//! The first request is [`Request::Hello`]; the host answers with its protocol version and the
//! emulator core it runs. A request with id 0 wants no reply.
//!
//! MIT licensed: both sides use it.

use std::io::{self, Read, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

/// Bumped on any change to the messages below.
pub const PROTOCOL: u32 = 1;
/// A raw CD sector: sync, header, subheader, data, EDC/ECC.
pub const RAW_SECTOR: usize = 2352;
pub const KIND_MESSAGE: u8 = 0;
pub const KIND_FRAME: u8 = 1;
/// Messages larger than this are refused (a whole-disc patch set is far smaller).
pub const MAX_MESSAGE: usize = 1 << 30;

/// A request with its id; replies carry the same id. Id 0: no reply.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Envelope {
    pub id: u32,
    pub request: Request,
}

/// What the application asks of the host.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Request {
    /// The first message: answered with [`Answer::Hello`].
    Hello { protocol: u32 },
    /// Opens a disc and boots it (paused): the raw (2352-byte sector) image at `image`, `sectors`
    /// long (at least the image's; past it, unpatched sectors read as postgap), with `patches` laid
    /// over it, and `card` (128 KB, or empty for a fresh one) in memory card slot 1.
    Open { image: String, sectors: u32, patches: Vec<Sector>, card: Vec<u8> },
    /// Changes the sectors laid over the image: `set` replaces or adds, `clear` puts the image's back.
    /// The next CD read sees them.
    Patch { set: Vec<Sector>, clear: Vec<u32> },
    /// Runs in real time (paced by the emulated clock, with sound).
    Run,
    /// Stops running in real time (sound pauses; requests still work).
    Pause,
    /// Boots again from the disc as it is now (the card is kept).
    Reset,
    /// The pad held in port 1 now (bits: SELECT 0, START 3, UP 4, RIGHT 5, DOWN 6, LEFT 7, L2 8,
    /// R2 9, L1 10, R1 11, TRIANGLE 12, CIRCLE 13, CROSS 14, SQUARE 15). Usually sent with id 0.
    Pad { buttons: u16 },
    /// Holds `buttons` (on top of [`Request::Pad`]) for the next `frames` frames; answered with
    /// [`Answer::Frame`] once they are let go.
    Press { buttons: u16, frames: u32 },
    /// Runs one frame per entry of `pads`, as fast as possible (no pacing, no sound), with that
    /// pad held; answered with [`Answer::Frame`]. With `show_last` the last frame is sent as a
    /// picture to frame subscribers.
    Advance { pads: Vec<u16>, show_last: bool },
    /// Reads and writes RAM between two frames, all at once (see [`RamOp`]); answered with
    /// [`Answer::Batch`].
    Batch { ops: Vec<RamOp> },
    /// The whole machine (not the disc, not the memory card): answered with [`Answer::State`].
    SaveState,
    /// Restores a [`Answer::State`] made by the same core build; the disc and the card stay.
    LoadState { frame: u64, state: Vec<u8> },
    /// The memory card as the game last wrote it: [`Answer::Card`].
    Card,
    /// Puts a memory card (128 KB, or empty for a fresh one) in slot 1 of the running machine.
    InsertCard { card: Vec<u8> },
    /// The picture now: [`Answer::Picture`].
    Screenshot,
    /// Sends every `every`-th frame as a picture while running (0: none).
    Frames { every: u32 },
    /// Sound volume (0..1, on a perceptual curve) and mute.
    Audio { volume: f32, muted: bool },
    /// Health counters: [`Answer::Diag`].
    Diag,
    /// Answers with the final [`Answer::Card`], then exits.
    Shutdown,
}

/// A sector laid over the image.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Sector {
    pub lba: u32,
    /// [`RAW_SECTOR`] bytes.
    pub raw: Vec<u8>,
}

/// One step of a [`Request::Batch`]. Every `Guard` is checked first; if any doesn't match, no
/// `Write` happens (the answer says so). Reads see RAM as it was before the batch's writes.
/// Addresses are CPU addresses: main RAM in any segment, or the scratchpad.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum RamOp {
    Read { addr: u32, len: u32 },
    /// The bytes at `addr` must still be these for the batch's writes to happen.
    Guard { addr: u32, bytes: Vec<u8> },
    Write { addr: u32, bytes: Vec<u8> },
}

/// A reply's content.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Answer {
    Hello {
        protocol: u32,
        /// The emulator core and build, e.g. "psoxide-d4d8006": machine states only load into the same one.
        core: String,
        /// This program's name and version.
        host: String,
    },
    Ok,
    /// Frames run since boot; for an [`Request::Advance`] also the ms the core took for them, the
    /// loudest sample the sound reached (0..32768, to tell silence from sound without a device) and
    /// the display's size after the last one.
    Frame { frame: u64, emu_ms: f64, audio_peak: u16, width: u32, height: u32 },
    /// The reads, in order, whether the writes happened, and the frame it ran after.
    Batch { reads: Vec<Vec<u8>>, applied: bool, frame: u64 },
    State { frame: u64, state: Vec<u8> },
    Card(Vec<u8>),
    Picture(Picture),
    Diag(Diag),
}

/// A picture: RGBA8 rows, `width` × `height`, as the TV shows it (interlaced modes woven).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// What the host sends without being asked, and replies.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Event {
    Reply { id: u32, result: Result<Answer, String> },
    /// Once a second while open.
    Stats(Stats),
    /// The game wrote its memory card (sent within half a second of the write): the whole card.
    Card(Vec<u8>),
    /// The core failed and stopped running.
    Halted { error: String },
    Log(String),
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    pub open: bool,
    pub running: bool,
    pub frames: u64,
    /// Over the last second.
    pub fps: f64,
    /// Frames per emulated second (NTSC: about 59.29).
    pub field_hz: f64,
    /// Ms per frame running the core (average, worst), and copying the picture out.
    pub emu_ms: f64,
    pub emu_ms_max: f64,
    pub copy_ms: f64,
    pub width: u32,
    pub height: u32,
    pub audio: Option<AudioStats>,
    /// Why there's no sound output (no device, a format it can't play, …).
    pub audio_error: Option<String>,
    /// Sectors the core read, and how many came from patches.
    pub reads: u64,
    pub patched_reads: u64,
    /// The disc's length and how many sectors are patched.
    pub sectors: u32,
    pub patched: u32,
    /// Pictures sent, and dropped because the application wasn't reading.
    pub pictures_sent: u64,
    pub pictures_dropped: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct AudioStats {
    pub device: String,
    pub rate: u32,
    pub channels: u16,
    /// Sound queued now.
    pub queued_ms: f64,
    pub underruns: u64,
    /// Samples dropped because the queue was full.
    pub overruns: u64,
    /// The current resample nudge, in parts per million.
    pub adjust_ppm: f64,
    /// SPU samples handed in, and device frames played, since the stream opened.
    pub fed: u64,
    pub played: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Diag {
    /// MDEC macroblocks decoded since boot (movies play through the MDEC).
    pub mdec_macroblocks: u64,
    /// CD sectors the drive delivered with nobody reading them.
    pub cd_dropped: u64,
    /// The first kernel call the core's HLE BIOS doesn't implement, if the game made one.
    pub first_unimplemented: Option<String>,
    pub cycles: u64,
}

/// A picture's header on the wire (kind [`KIND_FRAME`]), then `width * height * 4` bytes of RGBA8.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct FrameHeader {
    /// Frames run since boot.
    pub frame: u64,
    /// Emulated seconds since boot at the frame's end (a playout clock).
    pub emulated: f64,
    /// Ms the core took for this frame.
    pub emu_ms: f32,
    pub width: u16,
    pub height: u16,
    /// Wall-clock ms since the Unix epoch when the host sent it (for latency).
    pub sent_at: f64,
}

impl FrameHeader {
    pub const LEN: usize = 8 + 8 + 4 + 2 + 2 + 8;

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..8].copy_from_slice(&self.frame.to_le_bytes());
        b[8..16].copy_from_slice(&self.emulated.to_le_bytes());
        b[16..20].copy_from_slice(&self.emu_ms.to_le_bytes());
        b[20..22].copy_from_slice(&self.width.to_le_bytes());
        b[22..24].copy_from_slice(&self.height.to_le_bytes());
        b[24..32].copy_from_slice(&self.sent_at.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> Option<Self> {
        let b: &[u8; Self::LEN] = b.get(..Self::LEN)?.try_into().ok()?;
        let u64_at = |i: usize| u64::from_le_bytes(b[i..i + 8].try_into().expect("8 bytes"));
        Some(Self {
            frame: u64_at(0),
            emulated: f64::from_bits(u64_at(8)),
            emu_ms: f32::from_le_bytes(b[16..20].try_into().expect("4 bytes")),
            width: u16::from_le_bytes([b[20], b[21]]),
            height: u16::from_le_bytes([b[22], b[23]]),
            sent_at: f64::from_bits(u64_at(24)),
        })
    }
}

/// What [`read`] got: a message, or a picture.
#[derive(Debug)]
pub enum Incoming<T> {
    Message(T),
    Frame(FrameHeader, Vec<u8>),
}

fn bad(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

/// Writes one message (an [`Envelope`] or an [`Event`]).
pub fn write_message<W: Write, T: Serialize>(w: &mut W, msg: &T) -> io::Result<()> {
    let body = postcard::to_allocvec(msg).map_err(bad)?;
    w.write_all(&((body.len() + 1) as u32).to_le_bytes())?;
    w.write_all(&[KIND_MESSAGE])?;
    w.write_all(&body)?;
    w.flush()
}

/// Writes one picture.
pub fn write_frame<W: Write>(w: &mut W, header: &FrameHeader, rgba: &[u8]) -> io::Result<()> {
    w.write_all(&((1 + FrameHeader::LEN + rgba.len()) as u32).to_le_bytes())?;
    w.write_all(&[KIND_FRAME])?;
    w.write_all(&header.encode())?;
    w.write_all(rgba)?;
    w.flush()
}

/// Reads the next message or picture; `None` when the other side closed the pipe between messages.
pub fn read<R: Read, T: DeserializeOwned>(r: &mut R) -> io::Result<Option<Incoming<T>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len == 0 || len > MAX_MESSAGE {
        return Err(bad(format!("a message of {len} bytes")));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    match body[0] {
        KIND_MESSAGE => postcard::from_bytes(&body[1..]).map(|m| Some(Incoming::Message(m))).map_err(bad),
        KIND_FRAME => {
            let header = FrameHeader::decode(&body[1..]).ok_or_else(|| bad("a short picture header"))?;
            body.drain(..1 + FrameHeader::LEN);
            Ok(Some(Incoming::Frame(header, body)))
        }
        k => Err(bad(format!("message kind {k}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_and_pictures_round_trip() {
        let mut wire = Vec::new();
        let req = Envelope { id: 7, request: Request::Batch { ops: vec![RamOp::Read { addr: 0x1F80_0000, len: 256 }, RamOp::Guard { addr: 0x1F80_0051, bytes: vec![0] }, RamOp::Write { addr: 0x1F80_0050, bytes: vec![78, 0x80] }] } };
        write_message(&mut wire, &req).unwrap();
        let header = FrameHeader { frame: 4414, emulated: 74.44, emu_ms: 3.5, width: 512, height: 224, sent_at: 1.79e12 };
        let rgba = vec![9u8; 512 * 224 * 4];
        write_frame(&mut wire, &header, &rgba).unwrap();
        let mut r = &wire[..];
        match read::<_, Envelope>(&mut r).unwrap() {
            Some(Incoming::Message(Envelope { id: 7, request: Request::Batch { ops } })) => assert_eq!(ops.len(), 3),
            other => panic!("{other:?}"),
        }
        match read::<_, Envelope>(&mut r).unwrap() {
            Some(Incoming::Frame(h, px)) => {
                assert_eq!(h, header);
                assert_eq!(px.len(), rgba.len());
            }
            other => panic!("{other:?}"),
        }
        assert!(read::<_, Envelope>(&mut r).unwrap().is_none(), "a clean end");
    }

    #[test]
    fn a_cut_message_is_an_error_not_an_end() {
        let mut wire = Vec::new();
        write_message(&mut wire, &Event::Log("hello".into())).unwrap();
        wire.truncate(wire.len() - 2);
        assert!(read::<_, Event>(&mut &wire[..]).is_err());
    }
}
