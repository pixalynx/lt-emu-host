//! LT-103's acceptance checks for code hooks, on the retail USA disc of Guardian's Crusade.
//!
//! ```text
//! cargo run --release -p lt-emu-hooks --example hooks_check -- <disc.bin> <out dir> [identity|replace|all] [frames]
//! ```
//!
//! **identity** (criteria 2, 4, 5): two machines boot the same disc with the same input.
//! - Machine A has no hooks. Machine B has 11 observe hooks on the engine's draw functions, the
//!   field's chunk streaming, the music sequencer (run from the VSync interrupt) and a battle
//!   function; the OVR02 ones are guarded by the word OVR02 has there.
//! - The script plays New Game to the village, walks it, opens and closes the field menu, warps to
//!   chunk 80, walks until a random battle, presses ✕ until it ends, then walks again.
//! - After every frame the two machines' main RAM, scratchpad and CPU registers must be equal. The
//!   pictures are compared every 60 frames.
//! - It also reports each hook's calls in the field and in battle (the guarded OVR02 hooks must
//!   never fire while OVR04 is loaded), and the time each machine spent running.
//!
//! **replace** (criterion 3): machine C replaces `WorldChunk_DrawObjectMesh` (it returns the packet
//! cursor it was given, `a2`, in `v0`), so no terrain packet reaches the GPU. The same script runs
//! to the village and walks it. The pictures of A and C are written for comparison, and the
//! check reports that C's field and Knight keep going.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use emulator_core::{fast_boot_disc, Bus, ButtonState, Cpu, Deinterlace};
use lt_emu_hooks::{Action, Call, Hook, Hooks};
use psx_iso::{Disc, TrackSource};

const START: u16 = 1 << 3;
const UP: u16 = 1 << 4;
const RIGHT: u16 = 1 << 5;
const DOWN: u16 = 1 << 6;
const LEFT: u16 = 1 << 7;
const TRIANGLE: u16 = 1 << 12;
const CROSS: u16 = 1 << 14;

/// The field context in the scratchpad, and the chunk slot table (MAP_LOADING §1.3).
const CTX: u32 = 0x1F80_0000;
const SLOTS: u32 = 0x801C_E800;
const MODE_BATTLE: u8 = 2;

/// `WorldChunk_DrawObjectMesh`'s first instruction in OVR02 (`sw $s6, 0x204($a3)`; the decomp's
/// `src/ovr02/handwritten/WorldChunk_DrawObjectMesh.s`).
const MESH_FIRST_WORD: u32 = 0xACF6_0204;

/// OVR04's `Camera_UpdateTask` (battle; it runs every battle frame) starts at 0x801FD83C, inside
/// the range OVR02 fills in the field, where OVR02 has a 0 word. The words come from the decomp's
/// `disk/OVR/OVR02.BIN` and `OVR04.BIN`.
const CAMERA_TASK: u32 = 0x801F_D83C;
const CAMERA_TASK_OVR04: u32 = 0x27BD_FFD0;
const CAMERA_TASK_OVR02: u32 = 0x0000_0000;

/// Where the roam stage warps to, in turn: a town, an interior, the overworld, home.
const ROAM: [u16; 4] = [39, 326, 114, 78];

/// The hooks of the identity check: (name, address, guarded by OVR02's word).
const HOOKS: [(&str, u32, bool); 11] = [
    ("WorldChunk_DrawObjects", 0x801E_080C, true),
    ("WorldChunk_DrawObjectMesh", 0x801E_090C, true),
    ("Npc_DrawIfVisible", 0x801E_9E90, true),
    ("Field_DrawTorchFlames", 0x801E_B4AC, true),
    ("Model_DrawParts", 0x8019_DD70, false),
    ("Model_DrawParts (second entry)", 0x8019_DD68, false),
    ("Gte_SetComposedTransform", 0x801A_11CC, false),
    ("Gte_SetRotTransMatrix", 0x801A_1338, false),
    ("Field_ChunkStreamTask", 0x8018_B2EC, false),
    ("Sequencer_TickUpdate", 0x8019_A178, false),
    ("FUN_80144b48 (battle actors)", 0x8014_4B48, false),
];

struct Shared(Arc<Vec<u8>>);

impl TrackSource for Shared {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> bool {
        let start = offset as usize;
        match self.0.get(start..start + out.len()) {
            Some(src) => {
                out.copy_from_slice(src);
                true
            }
            None => false,
        }
    }
}

struct Machine {
    cpu: Cpu,
    bus: Bus,
    hooks: Option<Hooks>,
    /// Time spent running frames (not checking them).
    busy: Duration,
}

impl Machine {
    fn boot(bin: &Arc<Vec<u8>>) -> Machine {
        let disc = Disc::from_source(Box::new(Shared(bin.clone())));
        let mut bus = Bus::new_without_bios();
        let mut cpu = Cpu::new();
        fast_boot_disc(&mut bus, &mut cpu, &disc).expect("the disc didn't boot");
        bus.cdrom.insert_disc(Some(disc));
        bus.attach_digital_pad_port1();
        bus.attach_memcard_port1(Vec::new());
        Machine { cpu, bus, hooks: None, busy: Duration::ZERO }
    }

    /// One frame with `pad` held, as the host program's `Machine::run_frame` (with hooks, through
    /// `Hooks::run_frame`).
    fn frame(&mut self, pad: u16, handler: &mut impl FnMut(&Call) -> Action) -> Result<(), String> {
        let t = Instant::now();
        self.bus.set_port1_buttons(ButtonState::from_bits(pad));
        match &mut self.hooks {
            Some(hooks) => {
                hooks.run_frame(&mut self.cpu, &mut self.bus, handler)?;
            }
            None => {
                let start = self.bus.irq().raise_counts()[0];
                let (_, r) = self.cpu.run(&mut self.bus, lt_emu_hooks::STEPS_PER_FRAME_CAP, u64::MAX, |b| b.irq().raise_counts()[0] != start);
                r.map_err(|e| format!("{e} (pc={:#010x})", self.cpu.pc()))?;
            }
        }
        self.bus.run_spu_to_current_cycle();
        self.bus.spu.discard_audio();
        self.busy += t.elapsed();
        Ok(())
    }

    fn read(&self, addr: u32, len: usize) -> Vec<u8> {
        lt_emu_hooks::read(&self.bus, addr, len)
    }

    fn write(&mut self, addr: u32, bytes: &[u8]) {
        for (i, &b) in bytes.iter().enumerate() {
            assert!(self.bus.write8_safe(addr + i as u32, b), "write {addr:#x}");
        }
    }

    fn field(&self) -> Field {
        let ctx = self.read(CTX, 0x100);
        let slots = self.read(SLOTS, 12);
        let id = |i: usize| u16::from_le_bytes([slots[i * 2], slots[i * 2 + 1]]);
        Field {
            cur: u16::from_le_bytes([ctx[0xE8], ctx[0xE9]]),
            ids: [id(0), id(1), id(2), id(3)],
            loaded: [slots[8], slots[9], slots[10], slots[11]],
            warp: ctx[0x51],
            transition: ctx[0xDF],
            mode: ctx[0x43],
        }
    }

    /// The Knight's x, z (`Character_Struct[0]` at 0x80146600: s16 x, h, z, as `lt_emu::gc::place`
    /// writes them).
    fn knight(&self) -> (i16, i16) {
        let b = self.read(0x8014_6600, 6);
        (i16::from_le_bytes([b[0], b[1]]), i16::from_le_bytes([b[4], b[5]]))
    }

    fn picture(&self) -> (Vec<u8>, u32, u32) {
        self.bus.gpu.display_rgba8_with(Deinterlace::Weave)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Field {
    cur: u16,
    ids: [u16; 4],
    loaded: [u8; 4],
    warp: u8,
    transition: u8,
    mode: u8,
}

impl Field {
    fn idle(&self) -> bool {
        self.warp < 0x80 && self.transition < 0x80 && self.ids.iter().zip(self.loaded).all(|(&id, ld)| id == 0xFFFF || ld != 0)
    }
}

/// The script: a stage and its frame count, deciding each frame's pad from machine A's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    NewGame,
    Village,
    Warp,
    Encounter,
    Battle,
    After,
    Walk,
    Roam,
    Done,
}

struct Script {
    stage: Stage,
    since: u64,
    idle_for: u32,
    log: Vec<String>,
    /// Ask both machines for a warp to this chunk before the next frame.
    warp_to: Option<u16>,
    /// The roam stage runs until this frame.
    roam_until: u64,
}

impl Script {
    fn new() -> Self {
        Script { stage: Stage::NewGame, since: 0, idle_for: 0, log: Vec::new(), warp_to: None, roam_until: 0 }
    }

    fn next(&mut self, frame: u64, stage: Stage) {
        self.log.push(format!("frame {frame}: {:?} -> {stage:?}", self.stage));
        self.stage = stage;
        self.since = frame;
        self.idle_for = 0;
    }

    /// The pad for `frame`, from the field state machine A has now.
    fn pad(&mut self, frame: u64, f: Field, replace_only: bool) -> u16 {
        let t = frame - self.since;
        let walk = |t: u64| [UP, RIGHT, DOWN, LEFT][((t / 90) % 4) as usize];
        let tap = |t: u64, every: u64, key: u16| if t % every < 6 { key } else { 0 };
        match self.stage {
            Stage::NewGame => {
                self.idle_for = if f.cur == 78 && f.idle() && f.mode != MODE_BATTLE && frame > 3000 { self.idle_for + 1 } else { 0 };
                if self.idle_for >= 60 {
                    self.next(frame, Stage::Village);
                    return 0;
                }
                if frame % 120 >= 8 {
                    0
                } else if frame < 2300 {
                    START
                } else {
                    CROSS
                }
            }
            Stage::Village => {
                if t >= 2400 {
                    self.next(frame, if replace_only { Stage::Done } else { Stage::Warp });
                    return 0;
                }
                // The field menu: open at 1200, close at 1500.
                if (1200..1206).contains(&t) {
                    return START;
                }
                if (1500..1506).contains(&t) {
                    return TRIANGLE;
                }
                if (1200..1560).contains(&t) {
                    return 0;
                }
                walk(t) | tap(t, 200, CROSS)
            }
            Stage::Warp => {
                if f.cur == 80 && f.idle() {
                    self.idle_for += 1;
                    if self.idle_for >= 30 {
                        self.next(frame, Stage::Encounter);
                    }
                } else if f.idle() && f.cur != 80 && self.warp_to.is_none() && t < 600 {
                    self.warp_to = Some(80);
                }
                0
            }
            Stage::Encounter => {
                if f.mode == MODE_BATTLE {
                    self.next(frame, Stage::Battle);
                    return 0;
                }
                if t > 7200 {
                    self.log.push(format!("frame {frame}: no encounter in 2 minutes"));
                    self.next(frame, Stage::Walk);
                }
                walk(t)
            }
            Stage::Battle => {
                if f.mode != MODE_BATTLE {
                    self.next(frame, Stage::After);
                    return 0;
                }
                if t > 14_400 {
                    self.log.push(format!("frame {frame}: still in battle after 4 minutes"));
                    self.next(frame, Stage::Walk);
                }
                tap(t, 40, CROSS)
            }
            Stage::After => {
                self.idle_for = if f.mode != MODE_BATTLE && f.idle() { self.idle_for + 1 } else { 0 };
                if self.idle_for >= 60 || t > 3600 {
                    self.next(frame, Stage::Walk);
                }
                tap(t, 40, CROSS)
            }
            Stage::Walk => {
                if t >= 1800 {
                    self.next(frame, Stage::Roam);
                }
                walk(t)
            }
            // Until `roam_until`: a warp every 2,400 frames, walking between, any battle fought.
            Stage::Roam => {
                if frame >= self.roam_until {
                    self.next(frame, Stage::Done);
                    return 0;
                }
                if f.mode == MODE_BATTLE {
                    return tap(t, 40, CROSS);
                }
                if t % 2400 == 600 && f.idle() {
                    let to = ROAM[((t / 2400) % ROAM.len() as u64) as usize];
                    self.log.push(format!("frame {frame}: warp to {to}"));
                    self.warp_to = Some(to);
                }
                walk(t) | tap(t, 200, CROSS)
            }
            Stage::Done => 0,
        }
    }
}

/// `lt_warp`: ctx+0x50 = chunk | 0x8000, only while idle.
fn request_warp(m: &mut Machine, chunk: u16) {
    m.write(CTX + 0x50, &[(chunk & 0xFF) as u8, (chunk >> 8) as u8 | 0x80]);
}

fn scratchpad(m: &Machine) -> Vec<u8> {
    m.read(0x1F80_0000, 0x400)
}

fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    a.iter().zip(b).position(|(x, y)| x != y)
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| (h ^ u64::from(b)).wrapping_mul(0x100_0000_01b3))
}

fn write_png(path: &Path, (rgba, w, h): &(Vec<u8>, u32, u32)) {
    let file = std::fs::File::create(path).expect("png file");
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), *w, *h);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    e.write_header().expect("png header").write_image_data(rgba).expect("png data");
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        eprintln!("usage: hooks_check <disc.bin> <out dir> [identity|replace|all] [frames]");
        std::process::exit(2);
    }
    let bin = Arc::new(std::fs::read(&args[1]).expect("disc"));
    let out = PathBuf::from(&args[2]);
    std::fs::create_dir_all(&out).expect("out dir");
    let what = args.get(3).map(String::as_str).unwrap_or("all");
    let frames: u64 = args.get(4).map(|f| f.parse().expect("frames")).unwrap_or(30_000);
    let mut ok = true;
    if what == "identity" || what == "all" {
        ok &= identity(&bin, &out, frames);
    }
    if what == "replace" || what == "all" {
        ok &= replace(&bin, &out);
    }
    println!("{}", if ok { "PASS" } else { "FAIL" });
    std::process::exit(if ok { 0 } else { 1 });
}

/// Criteria 2, 4 and 5.
fn identity(bin: &Arc<Vec<u8>>, out: &Path, max_frames: u64) -> bool {
    let mut a = Machine::boot(bin);
    let mut b = Machine::boot(bin);
    // Guards: the word OVR02 has at each address, checked against the decomp for the mesh draw.
    // They're taken from the disc's OVR02 the first time the field runs (below), so the table is
    // installed then; until then B runs with no hooks, exactly as A.
    let mut script = Script::new();
    let mut in_battle: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut in_field: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut checked_pictures = 0;
    let mut report = String::new();
    let mut passed = true;
    let mut frame = 0u64;
    let mut battle_frames = 0u64;
    let mut ovr04_frames = 0u64;
    // Per hook: calls and guard skips in frames OVR04 was loaded from start to end.
    let mut in_ovr04: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    script.roam_until = max_frames;
    let t0 = Instant::now();
    while frame < max_frames && script.stage != Stage::Done {
        let f = a.field();
        // Install B's hooks once the field is up with OVR02 loaded (the village is reached).
        if b.hooks.is_none() && script.stage == Stage::Village {
            let mesh_word = u32::from_le_bytes(a.read(0x801E_090C, 4).try_into().unwrap());
            assert_eq!(mesh_word, MESH_FIRST_WORD, "OVR02 isn't loaded where the decomp says");
            let mut table: Vec<Hook> = HOOKS
                .iter()
                .map(|&(name, addr, guarded)| {
                    let h = Hook::observe(name, addr);
                    if guarded {
                        h.guarded(u32::from_le_bytes(a.read(addr, 4).try_into().unwrap()))
                    } else {
                        h
                    }
                })
                .collect();
            // One address, two overlays: each hook fires only with its own overlay loaded.
            let ovr02_there = u32::from_le_bytes(a.read(CAMERA_TASK, 4).try_into().unwrap());
            assert_eq!(ovr02_there, CAMERA_TASK_OVR02, "OVR02's word at the battle camera task");
            table.push(Hook::observe("OVR02 hook at 0x801FD83C", CAMERA_TASK).guarded(CAMERA_TASK_OVR02));
            table.push(Hook::observe("Camera_UpdateTask (OVR04)", CAMERA_TASK).guarded(CAMERA_TASK_OVR04));
            let hooks = Hooks::new(table).expect("hook table");
            hooks.install(&mut b.cpu);
            b.hooks = Some(hooks);
            let _ = writeln!(report, "hooks installed on B at frame {frame}");
        }
        let pad = script.pad(frame, f, false);
        if let Some(chunk) = script.warp_to.take() {
            request_warp(&mut a, chunk);
            request_warp(&mut b, chunk);
        }
        let battle = f.mode == MODE_BATTLE;
        if battle {
            battle_frames += 1;
        }
        let ovr04_at = |m: &Machine| u32::from_le_bytes(m.read(CAMERA_TASK, 4).try_into().unwrap()) == CAMERA_TASK_OVR04;
        let ovr04_before = ovr04_at(&a);
        let before = b.hooks.as_ref().map(|h| h.counts.clone());
        if let Err(e) = a.frame(pad, &mut |_| Action::Continue) {
            let _ = writeln!(report, "A failed at frame {frame}: {e}");
            passed = false;
            break;
        }
        if let Err(e) = b.frame(pad, &mut |_| Action::Continue) {
            let _ = writeln!(report, "B failed at frame {frame}: {e}");
            passed = false;
            break;
        }
        let ovr04 = ovr04_before && ovr04_at(&a);
        if ovr04 {
            ovr04_frames += 1;
        }
        if let (Some(before), Some(h)) = (before, &b.hooks) {
            for (i, hook) in h.hooks().iter().enumerate() {
                let calls = h.counts.calls[i] - before.calls[i];
                let guarded = h.counts.guarded[i] - before.guarded[i];
                let e = (if battle { &mut in_battle } else { &mut in_field }).entry(hook.name.clone()).or_default();
                e.0 += calls;
                e.1 += guarded;
                if ovr04 {
                    let e = in_ovr04.entry(hook.name.clone()).or_default();
                    e.0 += calls;
                    e.1 += guarded;
                }
            }
        }
        // The comparison.
        let (ra, rb) = (a.bus.ram(), b.bus.ram());
        if let Some(at) = first_difference(ra, rb) {
            let _ = writeln!(report, "RAM differs at frame {frame}, {:#010x}: A {:#04x}, B {:#04x}", 0x8000_0000 + at as u32, ra[at], rb[at]);
            passed = false;
            break;
        }
        if let Some(at) = first_difference(&scratchpad(&a), &scratchpad(&b)) {
            let _ = writeln!(report, "scratchpad differs at frame {frame}, +{at:#x}");
            passed = false;
            break;
        }
        if a.cpu.pc() != b.cpu.pc() || a.cpu.gprs() != b.cpu.gprs() || a.bus.cycles() != b.bus.cycles() {
            let _ = writeln!(report, "CPU differs at frame {frame}: pc {:#010x} vs {:#010x}, cycles {} vs {}", a.cpu.pc(), b.cpu.pc(), a.bus.cycles(), b.bus.cycles());
            passed = false;
            break;
        }
        if frame.is_multiple_of(60) {
            if fnv(&a.picture().0) != fnv(&b.picture().0) {
                let _ = writeln!(report, "pictures differ at frame {frame}");
                passed = false;
                break;
            }
            checked_pictures += 1;
        }
        if frame.is_multiple_of(2000) {
            eprintln!("frame {frame} {:?} chunk {} mode {} ({:.0} s)", script.stage, f.cur, f.mode, t0.elapsed().as_secs_f64());
        }
        frame += 1;
    }
    let reached_battle = battle_frames > 0;
    let _ = writeln!(report, "{frame} frames ({} game ticks) compared every frame; {checked_pictures} pictures compared", frame / 2);
    for l in &script.log {
        let _ = writeln!(report, "  {l}");
    }
    let _ = writeln!(report, "battle (ctx+0x43 = 2): {battle_frames} frames; OVR04 loaded all frame long (its word at 0x801FD83C): {ovr04_frames} frames");
    let _ = writeln!(
        report,
        "{:<34} {:>11} {:>11} {:>11} {:>11} {:>11} {:>11}",
        "hook", "field", "field skip", "battle", "battle skip", "OVR04", "OVR04 skip"
    );
    let mut guard_ok = ovr04_frames > 0;
    let names: Vec<(String, u32, bool)> = b
        .hooks
        .as_ref()
        .map(|h| h.hooks().iter().map(|k| (k.name.clone(), k.addr, k.guard.is_some())).collect())
        .unwrap_or_default();
    for (name, addr, guarded) in &names {
        let fi = in_field.get(name).copied().unwrap_or_default();
        let ba = in_battle.get(name).copied().unwrap_or_default();
        let ov = in_ovr04.get(name).copied().unwrap_or_default();
        let _ = writeln!(
            report,
            "{name:<34} {:>11} {:>11} {:>11} {:>11} {:>11} {:>11}   {addr:#010x}{}",
            fi.0,
            fi.1,
            ba.0,
            ba.1,
            ov.0,
            ov.1,
            if *guarded { " guarded" } else { "" }
        );
        // OVR02's guarded hooks must stay silent while OVR04 is there; OVR04's must fire.
        let ovr02_guarded = *guarded && name != "Camera_UpdateTask (OVR04)";
        if ovr02_guarded && ov.0 != 0 {
            guard_ok = false;
        }
        if name == "Camera_UpdateTask (OVR04)" && ov.0 == 0 {
            guard_ok = false;
        }
        if name == "OVR02 hook at 0x801FD83C" && ov.1 == 0 {
            guard_ok = false;
        }
    }
    let (ta, tb) = (a.busy.as_secs_f64(), b.busy.as_secs_f64());
    let ratio = tb / ta;
    let _ = writeln!(report, "time running: A (no hooks) {ta:.2} s, B (13 hooks) {tb:.2} s: B/A = {ratio:.3} ({:.2} vs {:.2} ms a frame)", 1000.0 * ta / frame as f64, 1000.0 * tb / frame as f64);
    let c2 = passed && frame >= 20_000;
    let c4 = passed && reached_battle && guard_ok;
    let _ = writeln!(report, "hooks in the table: {}", names.len());
    let c5 = passed && ratio <= 1.10;
    let _ = writeln!(report, "criterion 2 (identical, 10,000 ticks): {}", if c2 { "PASS" } else { "FAIL" });
    let _ = writeln!(report, "criterion 4 (guarded hooks silent in battle): {}", if c4 { "PASS" } else { "FAIL" });
    let _ = writeln!(report, "criterion 5 (within 10 % of no hooks): {}", if c5 { "PASS" } else { "FAIL" });
    print!("{report}");
    std::fs::write(out.join("identity.txt"), &report).expect("report");
    c2 && c4 && c5
}

/// Criterion 3.
fn replace(bin: &Arc<Vec<u8>>, out: &Path) -> bool {
    let mut a = Machine::boot(bin);
    let mut c = Machine::boot(bin);
    let hooks = Hooks::new(vec![Hook::replace("WorldChunk_DrawObjectMesh", 0x801E_090C).guarded(MESH_FIRST_WORD)]).expect("table");
    hooks.install(&mut c.cpu);
    c.hooks = Some(hooks);
    let mut skip = |call: &Call| Action::Return(Some(call.arg(2)));
    let mut script = Script::new();
    let mut report = String::new();
    let mut frame = 0u64;
    let mut c_knight = Vec::new();
    let mut shot = 0;
    let mut alive = true;
    while script.stage != Stage::Done && frame < 12_000 {
        let f = a.field();
        let pad = script.pad(frame, f, true);
        if let Err(e) = a.frame(pad, &mut |_| Action::Continue) {
            let _ = writeln!(report, "A failed at frame {frame}: {e}");
            break;
        }
        if let Err(e) = c.frame(pad, &mut skip) {
            let _ = writeln!(report, "C failed at frame {frame}: {e}");
            alive = false;
            break;
        }
        if script.stage == Stage::Village {
            c_knight.push(c.knight());
            let t = frame - script.since;
            if t == 300 || t == 1000 || t == 2300 {
                write_png(&out.join(format!("replace-{shot}-original.png")), &a.picture());
                write_png(&out.join(format!("replace-{shot}-no-terrain.png")), &c.picture());
                shot += 1;
            }
        }
        frame += 1;
    }
    let calls = c.hooks.as_ref().map(|h| (h.counts.calls[0], h.counts.returned[0])).unwrap_or_default();
    let fc = c.field();
    let moved = c_knight.windows(2).filter(|w| w[0] != w[1]).count();
    let distinct: std::collections::BTreeSet<_> = c_knight.iter().collect();
    let differ = a.bus.ram().iter().zip(c.bus.ram()).filter(|(x, y)| x != y).count();
    let _ = writeln!(report, "{frame} frames; C replaced WorldChunk_DrawObjectMesh {} times ({} returns)", calls.0, calls.1);
    let _ = writeln!(report, "C at the end: chunk {}, mode {}, idle {}; the Knight moved on {moved} of {} village frames, {} places", fc.cur, fc.mode, fc.idle(), c_knight.len(), distinct.len());
    let _ = writeln!(report, "RAM bytes that differ between A and C at the end: {differ} (packet buffers, the ordering table, and anything timing moved)");
    let _ = writeln!(report, "pictures: {shot} pairs, replace-N-original.png / replace-N-no-terrain.png");
    let pass = alive && calls.1 > 0 && calls.0 == calls.1 && fc.cur == 78 && moved > 100 && shot == 3;
    let _ = writeln!(report, "criterion 3 (terrain gone, the game carries on): {}", if pass { "PASS" } else { "FAIL" });
    print!("{report}");
    std::fs::write(out.join("replace.txt"), &report).expect("report");
    pass
}
