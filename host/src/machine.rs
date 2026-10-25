//! PSoXide's core behind a small API: boot a disc, run a frame with the pad held, read and write
//! RAM, save and restore the machine. The core needs no BIOS: its HLE kernel boots the disc's EXE.

use std::sync::Arc;

use emulator_core::snapshot::{EmulatorState, EmulatorStateRef};
use emulator_core::{fast_boot_disc, Bus, ButtonState, Cpu, Deinterlace};
use lt_emu_protocol::{Diag, Picture};
use psx_iso::Disc;

use crate::disc::{Overlay, Source};

/// Instructions one frame may run before the core is called stuck (a frame is ~565,000 cycles;
/// PSoXide's own runner uses the same cap).
const STEPS_PER_FRAME_CAP: u64 = 1_000_000;
/// The CPU clock, for emulated time.
pub const CPU_HZ: f64 = 33_868_800.0;
/// The core build machine states are good for (`Cargo.toml` pins this rev).
pub const CORE: &str = "psoxide-d4d8006";

pub struct Machine {
    cpu: Cpu,
    bus: Bus,
    overlay: Arc<Overlay>,
    /// VBlanks run since boot.
    frame: u64,
    /// The memory card's last written bytes (machine states leave the card out).
    card: Vec<u8>,
}

impl Machine {
    /// Boots the disc in `overlay`: the EXE is loaded and the CPU sits at its entry point, with a
    /// digital pad and `card` (128 KB, or a fresh card) in port 1.
    pub fn open(overlay: Arc<Overlay>, card: Vec<u8>) -> Result<Self, String> {
        let (cpu, mut bus) = boot(&overlay)?;
        bus.attach_memcard_port1(card.clone());
        Ok(Self { cpu, bus, overlay, frame: 0, card })
    }

    /// Boots again from the disc as it is now (the card is kept).
    pub fn reset(&mut self) -> Result<(), String> {
        let (cpu, mut bus) = boot(&self.overlay)?;
        self.save_card();
        bus.attach_memcard_port1(self.card.clone());
        self.cpu = cpu;
        self.bus = bus;
        self.frame = 0;
        Ok(())
    }

    pub fn overlay(&self) -> &Arc<Overlay> {
        &self.overlay
    }

    pub fn frame_count(&self) -> u64 {
        self.frame
    }

    /// Emulated seconds since boot.
    pub fn seconds(&self) -> f64 {
        self.bus.cycles() as f64 / CPU_HZ
    }

    /// CPU cycles since boot. Frames are paced by the emulated time each covers: PSoXide's NTSC field
    /// is 59.29 Hz, so a fixed 59.94 Hz would play the game, and its sound, about 1 % fast.
    pub fn cycles(&self) -> u64 {
        self.bus.cycles()
    }

    /// Runs to the next VBlank with `pad` held (1 = pressed). With `keep_audio` false the SPU's
    /// output is dropped.
    pub fn run_frame(&mut self, pad: u16, keep_audio: bool) -> Result<(), String> {
        self.bus.set_port1_buttons(ButtonState::from_bits(pad));
        let start = self.bus.irq().raise_counts()[0];
        let (_, result) = self.cpu.run(&mut self.bus, STEPS_PER_FRAME_CAP, u64::MAX, |bus| bus.irq().raise_counts()[0] != start);
        self.bus.run_spu_to_current_cycle();
        if !keep_audio {
            self.bus.spu.discard_audio();
        }
        result.map_err(|e| {
            let c = self.cpu.cop0();
            format!("{e} (pc={:#010x} epc={:#010x})", self.cpu.pc(), c[14])
        })?;
        self.frame += 1;
        Ok(())
    }

    /// The SPU's stereo samples (44.1 kHz) since the last call.
    pub fn drain_audio(&mut self) -> Vec<(i16, i16)> {
        self.bus.spu.drain_audio()
    }

    /// The display as the TV would show it (interlaced modes woven).
    pub fn picture(&self) -> Picture {
        let (rgba, width, height) = self.bus.gpu.display_rgba8_with(Deinterlace::Weave);
        Picture { width, height, rgba }
    }

    /// The display's size now (what [`Machine::picture`] would give), without copying it.
    pub fn display_size(&self) -> (u32, u32) {
        let (_, w, h) = self.bus.gpu.display_rgba8_with(Deinterlace::Weave);
        (w, h)
    }

    /// Puts a memory card image (128 KB) in port 1; anything else is a fresh card.
    pub fn insert_card(&mut self, bytes: Vec<u8>) {
        self.bus.attach_memcard_port1(bytes.clone());
        self.card = bytes;
    }

    /// `len` bytes from `addr` (main RAM in any segment, or the scratchpad); unmapped bytes read 0.
    pub fn read_ram(&self, addr: u32, len: usize) -> Vec<u8> {
        let phys = addr & 0x1FFF_FFFF;
        let ram = self.bus.ram();
        if (phys as usize) < 0x80_0000 {
            let at = phys as usize % ram.len();
            if at + len <= ram.len() {
                return ram[at..at + len].to_vec();
            }
        }
        (0..len as u32).map(|i| self.bus.try_read8(addr.wrapping_add(i)).unwrap_or(0)).collect()
    }

    /// Writes `bytes` from `addr` (main RAM or the scratchpad). False if any byte fell outside them.
    pub fn write_ram(&mut self, addr: u32, bytes: &[u8]) -> bool {
        let mut ok = true;
        for (i, b) in bytes.iter().enumerate() {
            ok &= self.bus.write8_safe(addr.wrapping_add(i as u32), *b);
        }
        ok
    }

    pub fn diag(&self) -> Diag {
        Diag {
            mdec_macroblocks: self.bus.mdec.macroblocks_decoded(),
            cd_dropped: self.bus.cdrom.dropped_sectors(),
            first_unimplemented: self.bus.hle_bios_first_unimplemented().map(ToString::to_string),
            cycles: self.bus.cycles(),
        }
    }

    /// The memory card in port 1 as the game last wrote it (128 KB, or empty when it never was).
    pub fn card(&mut self) -> Vec<u8> {
        self.save_card();
        self.card.clone()
    }

    fn save_card(&mut self) {
        if let Some(card) = self.bus.memcard_port1_snapshot() {
            self.card = card;
        }
    }

    /// The card's bytes when the game wrote to it since the last call, else None.
    pub fn take_card_write(&mut self) -> Option<Vec<u8>> {
        let card = self.bus.memcard_port1_snapshot()?;
        self.card = card.clone();
        Some(card)
    }

    /// The whole machine (CPU, RAM, VRAM, SPU, CD drive position, timers) as the core's own serde
    /// layout (postcard): not the disc (a restore reads the overlay as it is then) and not the
    /// memory card (a card outlives states, as on other emulators).
    pub fn save_state(&self) -> Result<Vec<u8>, String> {
        postcard::to_allocvec(&EmulatorStateRef { cpu: &self.cpu, bus: &self.bus }).map_err(|e| e.to_string())
    }

    /// Replaces the machine with a [`Machine::save_state`] made by this core build, keeping this
    /// disc and the memory card as it is now.
    pub fn load_state(&mut self, frame: u64, state: &[u8]) -> Result<(), String> {
        let mut st: EmulatorState = postcard::from_bytes(state).map_err(|e| format!("the state doesn't read: {e}"))?;
        self.save_card();
        st.bus.restore_excluded_from(&mut self.bus);
        st.bus.attach_memcard_port1(self.card.clone());
        self.cpu = st.cpu;
        self.bus = st.bus;
        self.frame = frame;
        Ok(())
    }
}

fn boot(overlay: &Arc<Overlay>) -> Result<(Cpu, Bus), String> {
    let disc = Disc::from_source(Box::new(Source(overlay.clone())));
    let mut bus = Bus::new_without_bios();
    let mut cpu = Cpu::new();
    fast_boot_disc(&mut bus, &mut cpu, &disc).map_err(|e| format!("the disc didn't boot: {e:?}"))?;
    bus.cdrom.insert_disc(Some(disc));
    bus.attach_digital_pad_port1();
    bus.attach_memcard_port1(Vec::new());
    Ok((cpu, bus))
}
