//! lt-emu-host: a PlayStation emulator host program. It runs PSoXide's core
//! (<https://github.com/EBonura/PSoXide-emulator>, GPL-2.0-or-later) and is driven over its
//! stdin/stdout by the plain-data protocol in `lt-emu-protocol`: an application opens a disc
//! image (with sectors patched over it), runs it, sends the pad, reads and writes RAM between
//! frames, saves and restores the machine, and gets pictures, stats and memory card writes back.
//! Sound goes straight to the default output device. The work is the library's (`lib.rs`).
//!
//! `lt-emu-host --version` prints the version, the core build and the protocol.

use lt_emu_protocol::PROTOCOL;

fn main() {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("lt-emu-host {} ({}, protocol {PROTOCOL})", env!("CARGO_PKG_VERSION"), lt_emu_host::CORE);
        return;
    }
    let out = lt_emu_host::protocol_out();
    lt_emu_host::serve(out, Box::new(lt_emu_host::Plain), |_| {});
}
