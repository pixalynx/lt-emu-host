//! Research 16 (native remaster), a phase-0 sketch: the engine's draw functions as hook events.
//!
//! Boots the disc headless on PSoXide (no BIOS), plays New Game to the village with a START/✕
//! tape, then single-steps the CPU for a few frames and records every call to the given
//! addresses: the arguments, the return address, and optionally bytes read at an argument. This
//! is the "observe" mode of research 16 §6.2: nothing is written to the machine.
//!
//! ```text
//! cargo run --release --example draw_hooks -- <disc.bin> <out dir> <boot frames> <observe frames> <hook>...
//!   <hook> = name=0xADDR[:aN:len | :aN@off:len]...   e.g. Gte_SetComposedTransform=0x801A11CC:a0:0x20:a1:0x20
//! ```
//!
//! `aN@off:len` dumps `len` bytes at the pointer stored at `aN + off` (hex), as `aN@off_bytes`:
//! `Model_DrawParts=0x8019DD70:a0:0x3C:a0@2c:0x1000` logs a part record and its live vertices.
//! LT-108's `lt-scene match` reads such a log.
//!
//! Writes `<out dir>/hooks.jsonl` (one line per call), `<out dir>/summary.txt` (calls per frame
//! per hook) and `<out dir>/frame.rgba` + `frame.txt` (the last observed frame, width × height).
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;

use emulator_core::{fast_boot_disc, Bus, ButtonState, Cpu, Deinterlace};
use psx_iso::Disc;

const START: u16 = 1 << 3;
const CROSS: u16 = 1 << 14;
const STEPS_PER_FRAME_CAP: u64 = 2_000_000;

struct Hook {
    name: String,
    /// Register index, an optional offset to a pointer there, and the byte count to read.
    dumps: Vec<(u8, Option<u32>, usize)>,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 6 {
        eprintln!("usage: draw_hooks <disc.bin> <out dir> <boot frames> <observe frames> name=0xADDR[:aN:len]...");
        std::process::exit(2);
    }
    let out = std::path::PathBuf::from(&args[2]);
    std::fs::create_dir_all(&out).expect("out dir");
    let boot: u64 = args[3].parse().expect("boot frames");
    let observe: u64 = args[4].parse().expect("observe frames");
    let mut hooks: BTreeMap<u32, Hook> = BTreeMap::new();
    for spec in &args[5..] {
        let (name, rest) = spec.split_once('=').expect("name=0xADDR");
        let mut parts = rest.split(':');
        let addr = u32::from_str_radix(parts.next().unwrap().trim_start_matches("0x"), 16).expect("hex address");
        let mut dumps = Vec::new();
        while let (Some(r), Some(n)) = (parts.next(), parts.next()) {
            let (r, deref) = match r.split_once('@') {
                Some((r, off)) => (r, Some(u32::from_str_radix(off.trim_start_matches("0x"), 16).expect("hex offset"))),
                None => (r, None),
            };
            let reg = 4 + r.trim_start_matches('a').parse::<u8>().expect("aN");
            let len = usize::from_str_radix(n.trim_start_matches("0x"), 16).expect("hex length");
            dumps.push((reg, deref, len));
        }
        hooks.insert(addr, Hook { name: name.to_string(), dumps });
    }

    let bytes = std::fs::read(&args[1]).expect("disc");
    let disc = Disc::from_bin(bytes);
    let mut bus = Bus::new_without_bios();
    let mut cpu = Cpu::new();
    fast_boot_disc(&mut bus, &mut cpu, &disc).expect("boot");
    bus.cdrom.insert_disc(Some(disc));
    bus.attach_digital_pad_port1();
    bus.attach_memcard_port1(Vec::new());

    // New Game: START pulses through the opening, then ✕ through the title, name entry and the
    // bedroom (compat_tour's tape, from power-on: the village comes at about frame 3,640).
    let tape = |f: u64| -> u16 {
        if f % 120 >= 8 {
            0
        } else if f < 2300 {
            START
        } else {
            CROSS
        }
    };
    let t0 = std::time::Instant::now();
    for f in 0..boot {
        bus.set_port1_buttons(ButtonState::from_bits(tape(f)));
        let start = bus.irq().raise_counts()[0];
        let (_, r) = cpu.run(&mut bus, STEPS_PER_FRAME_CAP, u64::MAX, |b| b.irq().raise_counts()[0] != start);
        r.unwrap_or_else(|e| panic!("frame {f}: {e} (pc={:#010x})", cpu.pc()));
        bus.run_spu_to_current_cycle();
        bus.spu.discard_audio();
    }
    eprintln!("booted {boot} frames in {:.1} s", t0.elapsed().as_secs_f64());

    let mut log = std::fs::File::create(out.join("hooks.jsonl")).expect("log");
    let mut counts: BTreeMap<(u64, String), u32> = BTreeMap::new();
    let t1 = std::time::Instant::now();
    let mut stepped = 0u64;
    for f in 0..observe {
        bus.set_port1_buttons(ButtonState::from_bits(0));
        let start = bus.irq().raise_counts()[0];
        let mut steps = 0;
        while bus.irq().raise_counts()[0] == start && steps < STEPS_PER_FRAME_CAP {
            let pc = cpu.pc();
            if let Some(h) = hooks.get(&pc) {
                let g = cpu.gprs();
                let mut line = format!(
                    "{{\"frame\":{},\"hook\":\"{}\",\"a0\":\"{:#010x}\",\"a1\":\"{:#010x}\",\"a2\":\"{:#010x}\",\"a3\":\"{:#010x}\",\"ra\":\"{:#010x}\",\"sp\":\"{:#010x}\"",
                    boot + f,
                    h.name,
                    g[4],
                    g[5],
                    g[6],
                    g[7],
                    g[31],
                    g[29]
                );
                for &(reg, deref, len) in &h.dumps {
                    let (at, key) = match deref {
                        Some(off) => {
                            let p = read(&bus, g[reg as usize].wrapping_add(off), 4);
                            (u32::from_le_bytes([p[0], p[1], p[2], p[3]]), format!("a{}@{off:x}", reg - 4))
                        }
                        None => (g[reg as usize], format!("a{}", reg - 4)),
                    };
                    let mut hex = String::with_capacity(len * 2);
                    for b in read(&bus, at, len) {
                        let _ = write!(hex, "{b:02x}");
                    }
                    let _ = write!(line, ",\"{key}_bytes\":\"{hex}\"");
                }
                line.push('}');
                writeln!(log, "{line}").expect("log");
                *counts.entry((boot + f, h.name.clone())).or_default() += 1;
            }
            cpu.step(&mut bus).unwrap_or_else(|e| panic!("frame {}: {e} (pc={pc:#010x})", boot + f));
            steps += 1;
        }
        stepped += steps;
        bus.run_spu_to_current_cycle();
        bus.spu.discard_audio();
    }
    let secs = t1.elapsed().as_secs_f64();

    let mut summary = format!("observed {observe} frames from {boot}: {stepped} instructions single-stepped in {secs:.2} s\n");
    for ((f, name), n) in &counts {
        let _ = writeln!(summary, "frame {f} {name}: {n}");
    }
    std::fs::write(out.join("summary.txt"), &summary).expect("summary");
    print!("{summary}");

    let (rgba, w, h) = bus.gpu.display_rgba8_with(Deinterlace::Weave);
    std::fs::write(out.join("frame.rgba"), &rgba).expect("frame");
    std::fs::write(out.join("frame.txt"), format!("{w} {h}\n")).expect("frame size");
}

fn read(bus: &Bus, addr: u32, len: usize) -> Vec<u8> {
    let phys = addr & 0x1FFF_FFFF;
    let ram = bus.ram();
    if (phys as usize) < 0x80_0000 {
        let at = phys as usize % ram.len();
        if at + len <= ram.len() {
            return ram[at..at + len].to_vec();
        }
    }
    (0..len as u32).map(|i| bus.try_read8(addr.wrapping_add(i)).unwrap_or(0)).collect()
}
