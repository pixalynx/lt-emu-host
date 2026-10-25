//! lt-emu-host: a PlayStation emulator host program. It runs PSoXide's core
//! (<https://github.com/EBonura/PSoXide-emulator>, GPL-2.0-or-later) and is driven over its
//! stdin/stdout by the plain-data protocol in `lt-emu-protocol`: an application opens a disc
//! image (with sectors patched over it), runs it, sends the pad, reads and writes RAM between
//! frames, saves and restores the machine, and gets pictures, stats and memory card writes back.
//! Sound goes straight to the default output device.
//!
//! `lt-emu-host --version` prints the version, the core build and the protocol.

mod audio;
mod disc;
mod machine;
mod runner;
mod thread;

use std::io::{BufReader, BufWriter, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};

use lt_emu_protocol::{Envelope, Event, Incoming, PROTOCOL};
use runner::{Out, Outgoing};

/// The protocol's output: the process's own stdout, taken before anything else can write to it.
/// Fd 1 is then pointed at stderr, because the core's HLE BIOS prints the game's TTY output
/// (printf, puts) to stdout, which would corrupt the protocol (and block on its lock).
fn protocol_out() -> std::fs::File {
    #[cfg(unix)]
    // SAFETY: plain fd duplication at startup, before any other thread exists.
    unsafe {
        use std::os::fd::FromRawFd;
        let fd = libc::dup(1);
        assert!(fd >= 0, "stdout can't be duplicated");
        libc::dup2(2, 1);
        std::fs::File::from_raw_fd(fd)
    }
    #[cfg(windows)]
    // SAFETY: swaps the standard handle at startup, before any other thread exists; Rust's stdout
    // asks for the handle on every write, so later prints go to stderr.
    unsafe {
        use std::os::windows::io::FromRawHandle;
        use windows_sys::Win32::System::Console::{GetStdHandle, SetStdHandle, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE};
        let out = GetStdHandle(STD_OUTPUT_HANDLE);
        SetStdHandle(STD_OUTPUT_HANDLE, GetStdHandle(STD_ERROR_HANDLE));
        std::fs::File::from_raw_handle(out as _)
    }
}

fn main() {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("lt-emu-host {} ({}, protocol {PROTOCOL})", env!("CARGO_PKG_VERSION"), machine::CORE);
        return;
    }
    let protocol_out = protocol_out();
    let (out_tx, out_rx) = mpsc::channel::<Outgoing>();
    let queued = Arc::new(AtomicUsize::new(0));
    let out = Out::new(out_tx, queued.clone());

    // stdout: events in order; a picture leaves the queue count when written.
    let writer = std::thread::Builder::new()
        .name("lt-emu-host-out".into())
        .spawn(move || {
            let mut w = BufWriter::with_capacity(1 << 20, protocol_out);
            for msg in out_rx {
                let r = match msg {
                    Outgoing::Event(e) => lt_emu_protocol::write_message(&mut w, &e),
                    Outgoing::Picture(h, rgba) => {
                        let r = lt_emu_protocol::write_frame(&mut w, &h, &rgba);
                        queued.fetch_sub(1, Ordering::Relaxed);
                        r
                    }
                };
                if r.is_err() {
                    // The application went away.
                    std::process::exit(0);
                }
            }
            let _ = w.flush();
        })
        .expect("spawn the writer");

    // stdin: requests to the emulator thread. A closed pipe (the application quit) ends it.
    let (tx, rx) = mpsc::channel::<Envelope>();
    let reader_out = out.clone();
    std::thread::Builder::new()
        .name("lt-emu-host-in".into())
        .spawn(move || {
            let mut r = BufReader::with_capacity(1 << 20, std::io::stdin().lock());
            loop {
                match lt_emu_protocol::read::<_, Envelope>(&mut r) {
                    Ok(Some(Incoming::Message(env))) => {
                        if tx.send(env).is_err() {
                            return;
                        }
                    }
                    Ok(Some(Incoming::Frame(..))) => reader_out.event(Event::Log("a picture sent to the host was ignored".into())),
                    Ok(None) => return,
                    Err(e) => {
                        reader_out.event(Event::Log(format!("unreadable request: {e}")));
                        return;
                    }
                }
            }
        })
        .expect("spawn the reader");

    runner::run(rx, out);
    // Shut down: let the writer drain what's queued (the final card).
    drop(writer);
    std::thread::sleep(std::time::Duration::from_millis(100));
}
