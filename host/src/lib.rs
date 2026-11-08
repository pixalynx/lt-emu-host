//! lt-emu-host as a library: the machine ([`machine::Machine`]), the patched disc
//! ([`disc::Overlay`]), sound ([`audio`]) and the protocol's server ([`serve`]), for the
//! `lt-emu-host` program and for another program that hosts the game itself. Living Tool
//! Remaster's `lt-remaster --host` serves the same protocol with its own [`machine::Stepper`]
//! (code hooks and a native renderer) and its own window.

pub mod audio;
pub mod disc;
pub mod machine;
pub mod runner;
pub mod thread;

use std::io::{BufReader, BufWriter, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};

use lt_emu_protocol::{Envelope, Event, Incoming};

pub use machine::{Plain, Stepper, CORE, CPU_HZ};
pub use runner::Out;
use runner::Outgoing;

/// The protocol's output: the process's own stdout, taken before anything else can write to it.
/// Fd 1 is then pointed at stderr, because the core's HLE BIOS prints the game's TTY output
/// (printf, puts) to stdout, which would corrupt the protocol (and block on its lock). Call it
/// first thing in `main`, before any other thread exists.
pub fn protocol_out() -> std::fs::File {
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

/// Serves the protocol: requests from this process's stdin, replies, events and pictures to
/// `protocol_out` (from [`protocol_out`]), frames run by `stepper` on the calling thread. Returns
/// after [`lt_emu_protocol::Request::Shutdown`], or when the application closes the pipe. `events`
/// gets a handle first, for events sent from elsewhere (a window's [`Event::Closed`]).
pub fn serve(protocol_out: std::fs::File, stepper: Box<dyn Stepper>, events: impl FnOnce(Out)) {
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

    events(out.clone());
    runner::run(rx, out, stepper);
    // Shut down: let the writer drain what's queued (the final card).
    drop(writer);
    std::thread::sleep(std::time::Duration::from_millis(100));
}
