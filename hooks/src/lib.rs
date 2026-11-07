//! Code hooks on PSoXide's CPU (Living Tool research 16 §6.2, LT-103).
//!
//! A [`Hooks`] table names code addresses. When the CPU reaches one, the run stops in front of it
//! and a [`Handler`] gets a [`Call`]: the registers as the function will see them and read access
//! to the machine. Then either the original code runs ([`Mode::Observe`]), or, for a
//! [`Mode::Replace`] hook, the handler may return from the function in its place
//! ([`Action::Return`]).
//!
//! The stop comes from PSoXide's `Cpu::set_code_hooks` (Living Tool's patch to the core): no
//! decoded block starts on or holds a hooked word, so everything else still runs batched, and a
//! machine with observe hooks only computes exactly what one without them does.
//!
//! Two overlays can load at one address (Guardian's Crusade's OVR02 and OVR04 at 0x801DF000), so a
//! hook can carry a guard: the instruction word it was written for. When RAM holds another word
//! there, the hook is skipped and counted in [`Counts::guarded`].
//!
//! A handler writes the game's memory only through [`Call::write`], applied when it returns (a
//! native replacement keeps the game's state as the original would). A replace hook also changes
//! what returning early changes: the PC, `v0`, and the load the call left pending.

use std::cell::RefCell;
use std::collections::HashMap;

pub use emulator_core::{Bus, Cpu};

/// Instructions one frame may run before the core is called stuck (the host program's cap).
pub const STEPS_PER_FRAME_CAP: u64 = 1_000_000;

const RAM_SIZE: usize = 2 * 1024 * 1024;

/// What a hook does with the function it stops at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Read, then let the original code run. The handler's [`Action`] is ignored.
    Observe,
    /// Read, then [`Action::Return`] from the function without running it, or
    /// [`Action::Continue`] to let it run this time.
    Replace,
}

/// One entry of the table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hook {
    /// A name for reports (the decomp's function name, say).
    pub name: String,
    /// The code address (any segment).
    pub addr: u32,
    /// The instruction word the hook was written for. When RAM holds another word at `addr`
    /// (another overlay in a shared slot), the hook doesn't fire.
    pub guard: Option<u32>,
    pub mode: Mode,
}

impl Hook {
    pub fn observe(name: impl Into<String>, addr: u32) -> Self {
        Hook { name: name.into(), addr, guard: None, mode: Mode::Observe }
    }

    pub fn replace(name: impl Into<String>, addr: u32) -> Self {
        Hook { name: name.into(), addr, guard: None, mode: Mode::Replace }
    }

    /// Fire only while `word` is the instruction at the hook's address.
    pub fn guarded(mut self, word: u32) -> Self {
        self.guard = Some(word);
        self
    }
}

/// What a handler wants done after a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// Run the original code.
    Continue,
    /// Return from the function without running it, with `v0` when given (replace hooks only).
    Return(Option<u32>),
}

/// The machine as a hook sees it: stopped in front of the hooked instruction.
pub struct Call<'a> {
    /// The hook's index in the table.
    pub hook: usize,
    pub name: &'a str,
    /// The PC the CPU stopped at.
    pub pc: u32,
    cpu: &'a Cpu,
    bus: &'a Bus,
    writes: RefCell<Vec<(u32, Vec<u8>)>>,
}

impl Call<'_> {
    /// Writes `bytes` at `addr` (main RAM or the scratchpad) when the handler returns, before the
    /// original code runs or the function returns.
    pub fn write(&self, addr: u32, bytes: &[u8]) {
        self.writes.borrow_mut().push((addr, bytes.to_vec()));
    }

    /// Register `r` as the instruction at the PC will read it (a load in a call's delay slot has
    /// landed).
    pub fn reg(&self, r: u8) -> u32 {
        self.cpu.settled_gpr(r)
    }

    /// Argument `i` of the call by the MIPS O32 convention: `a0`–`a3`, then the stack from
    /// `sp + 16`.
    pub fn arg(&self, i: u8) -> u32 {
        if i < 4 {
            self.reg(4 + i)
        } else {
            self.read_u32(self.sp().wrapping_add(4 * u32::from(i)))
        }
    }

    pub fn ra(&self) -> u32 {
        self.reg(31)
    }

    pub fn sp(&self) -> u32 {
        self.reg(29)
    }

    /// `len` bytes from `addr`: main RAM in any segment, or the scratchpad; 0 elsewhere.
    pub fn read(&self, addr: u32, len: usize) -> Vec<u8> {
        read(self.bus, addr, len)
    }

    pub fn read_u32(&self, addr: u32) -> u32 {
        let b = self.read(addr, 4);
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }

    pub fn read_u16(&self, addr: u32) -> u16 {
        let b = self.read(addr, 2);
        u16::from_le_bytes([b[0], b[1]])
    }

    pub fn cpu(&self) -> &Cpu {
        self.cpu
    }

    pub fn bus(&self) -> &Bus {
        self.bus
    }
}

/// Receives the calls. Any `FnMut(&Call) -> Action` is one.
pub trait Handler {
    fn call(&mut self, call: &Call) -> Action;
}

impl<F: FnMut(&Call) -> Action> Handler for F {
    fn call(&mut self, call: &Call) -> Action {
        self(call)
    }
}

/// Per hook, in table order.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Times the hook fired.
    pub calls: Vec<u64>,
    /// Times its address was reached with another instruction word there than its guard.
    pub guarded: Vec<u64>,
    /// Times a replace hook returned in the function's place.
    pub returned: Vec<u64>,
}

/// The table, installed on a CPU.
pub struct Hooks {
    hooks: Vec<Hook>,
    /// RAM word index → the hooks there, in table order.
    at: HashMap<usize, Vec<usize>>,
    pub counts: Counts,
}

impl Hooks {
    /// A table. Errors when an address is outside main RAM or not word-aligned, or when two hooks
    /// at one address could both fire (same or missing guards).
    pub fn new(hooks: Vec<Hook>) -> Result<Self, String> {
        let mut at: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, h) in hooks.iter().enumerate() {
            let word = ram_word(h.addr).ok_or_else(|| format!("{}: {:#010x} is not code in main RAM", h.name, h.addr))?;
            let list = at.entry(word).or_default();
            for &j in list.iter() {
                let other = &hooks[j];
                let both_replace = h.mode == Mode::Replace && other.mode == Mode::Replace;
                let overlap = match (h.guard, other.guard) {
                    (Some(a), Some(b)) => a == b,
                    _ => true,
                };
                if both_replace && overlap {
                    return Err(format!("{} and {} would both replace the function at {:#010x}", other.name, h.name, h.addr));
                }
            }
            list.push(i);
        }
        let n = hooks.len();
        Ok(Hooks { hooks, at, counts: Counts { calls: vec![0; n], guarded: vec![0; n], returned: vec![0; n] } })
    }

    pub fn hooks(&self) -> &[Hook] {
        &self.hooks
    }

    /// Sets the table's addresses as the CPU's code hooks.
    pub fn install(&self, cpu: &mut Cpu) {
        let addrs: Vec<u32> = self.hooks.iter().map(|h| h.addr).collect();
        cpu.set_code_hooks(&addrs);
    }

    /// Whether any hook is at code address `pc` (for callers that step the CPU themselves).
    pub fn covers(&self, pc: u32) -> bool {
        ram_word(pc).is_some_and(|w| self.at.contains_key(&w))
    }

    /// Removes every code hook from the CPU.
    pub fn uninstall(cpu: &mut Cpu) {
        cpu.set_code_hooks(&[]);
    }

    /// Runs to the next VBlank, as the host program's `Machine::run_frame` does, handling every
    /// hook stop on the way. Returns the instructions run.
    pub fn run_frame(&mut self, cpu: &mut Cpu, bus: &mut Bus, handler: &mut impl Handler) -> Result<u64, String> {
        let start = bus.irq().raise_counts()[0];
        let mut steps = 0u64;
        loop {
            let (ran, result) = cpu.run(bus, STEPS_PER_FRAME_CAP - steps, u64::MAX, |b| b.irq().raise_counts()[0] != start);
            steps += ran;
            result.map_err(|e| format!("{e} (pc={:#010x})", cpu.pc()))?;
            match cpu.take_code_hook_hit() {
                Some(pc) => {
                    self.handle(cpu, bus, pc, handler);
                    if steps >= STEPS_PER_FRAME_CAP {
                        return Err(format!("no VBlank in {STEPS_PER_FRAME_CAP} instructions (pc={:#010x})", cpu.pc()));
                    }
                }
                None => break,
            }
        }
        Ok(steps)
    }

    /// One stop at `pc`: every hook there whose guard holds is called in table order. A replace
    /// hook that returns ends the call (the function is skipped).
    pub fn handle(&mut self, cpu: &mut Cpu, bus: &mut Bus, pc: u32, handler: &mut impl Handler) {
        let Some(word) = ram_word(pc) else { return };
        let Some(list) = self.at.get(&word) else { return };
        let insn = ram_u32(bus, word * 4);
        let mut ret = None;
        let mut writes = Vec::new();
        for &i in list {
            let h = &self.hooks[i];
            if h.guard.is_some_and(|g| g != insn) {
                self.counts.guarded[i] += 1;
                continue;
            }
            self.counts.calls[i] += 1;
            let call = Call { hook: i, name: &h.name, pc, cpu, bus, writes: RefCell::new(Vec::new()) };
            let action = handler.call(&call);
            writes.extend(call.writes.into_inner());
            if let (Mode::Replace, Action::Return(v0)) = (h.mode, action) {
                self.counts.returned[i] += 1;
                ret = Some(v0);
                break;
            }
        }
        for (addr, bytes) in writes {
            for (k, b) in bytes.into_iter().enumerate() {
                bus.write8_safe(addr.wrapping_add(k as u32), b);
            }
        }
        if let Some(v0) = ret {
            cpu.return_from_code_hook(v0);
        }
    }
}

/// The RAM word index of a code address in any segment and mirror.
fn ram_word(addr: u32) -> Option<usize> {
    let phys = addr & 0x1FFF_FFFF;
    (addr & 3 == 0 && phys < 0x80_0000).then_some((phys as usize % RAM_SIZE) / 4)
}

fn ram_u32(bus: &Bus, at: usize) -> u32 {
    let ram = bus.ram();
    u32::from_le_bytes([ram[at], ram[at + 1], ram[at + 2], ram[at + 3]])
}

/// `len` bytes from `addr`: main RAM in any segment, or the scratchpad; unmapped bytes read 0.
pub fn read(bus: &Bus, addr: u32, len: usize) -> Vec<u8> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_cover_every_segment_and_mirror() {
        let w = ram_word(0x801E_090C).unwrap();
        assert_eq!(ram_word(0x001E_090C), Some(w));
        assert_eq!(ram_word(0xA01E_090C), Some(w));
        assert_eq!(ram_word(0x803E_090C), Some(w));
        assert_eq!(ram_word(0x801E_090E), None);
        assert_eq!(ram_word(0x1F80_0000), None);
    }

    #[test]
    fn two_replacements_need_different_guards() {
        let a = Hook::replace("a", 0x801E_090C);
        let b = Hook::replace("b", 0x801E_090C);
        assert!(Hooks::new(vec![a.clone(), b.clone()]).is_err());
        assert!(Hooks::new(vec![a.clone().guarded(1), b.clone().guarded(1)]).is_err());
        assert!(Hooks::new(vec![a.guarded(1), b.guarded(2)]).is_ok());
        assert!(Hooks::new(vec![Hook::observe("o", 0x801E_090C), Hook::replace("r", 0x801E_090C)]).is_ok());
    }

    #[test]
    fn addresses_outside_ram_are_refused() {
        assert!(Hooks::new(vec![Hook::observe("bios", 0xBFC0_0000)]).is_err());
        assert!(Hooks::new(vec![Hook::observe("odd", 0x8001_0002)]).is_err());
    }
}
