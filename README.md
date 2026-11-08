# lt-emu-host

A PlayStation emulator host program. It runs the core of
[PSoXide](https://github.com/EBonura/PSoXide-emulator) and is driven over its standard input and
output by a small plain-data protocol, so an application can run a PS1 game without linking an
emulator.

Living Tool uses it as its built-in emulator: it starts `lt-emu-host` when you press Play, sends
it the disc image path and the project's changed sectors, the pad and RAM reads and writes, and
shows the pictures it sends back. The program itself knows nothing about Living Tool or any game.

## What's here

| Path | What | Licence |
|---|---|---|
| `host/` | The program, and the same as a library (`lt_emu_host`): PSoXide's core, the disc (a raw image with patched sectors over it), the real-time loop, sound out (cpal), the pipe | GPL-3.0-or-later ([LICENSE](LICENSE)) |
| `protocol/` | `lt-emu-protocol`: the messages and their framing, shared by the program and its applications | MIT ([protocol/LICENSE](protocol/LICENSE)) |
| `hooks/` | `lt-emu-hooks`: code hooks on the core's CPU, a table of code addresses where a run stops so a handler can read the game, then let the original run or return in its place | GPL-3.0-or-later |

The core is PSoXide's `emulator-core` and `psx-iso`, GPL-2.0-or-later, at
[`d4d8006`](https://github.com/EBonura/PSoXide-emulator/tree/d4d8006ebe3c945ed4a568ae7ef77e9fc0a5028e)
(2026-10-24) plus one commit, built from the fork
[`pixalynx/PSoXide-emulator`](https://github.com/pixalynx/PSoXide-emulator/tree/lt-code-hooks)
at [`b27fff9`](https://github.com/pixalynx/PSoXide-emulator/commit/b27fff97dc9b265d776fb031ca0c920c0f243f95). Machine states only load
into the core build that saved them, so the rev only moves deliberately.

The added commit is `Cpu::set_code_hooks`, which `hooks/` uses: `Cpu::run` stops in front of a
hooked code address, and no decoded block starts on or holds one, so everything else still runs
batched. Without hooks set the core runs as d4d8006 does. The hooks are host configuration that
machine states leave out, so states load the same either way, and the core name stays
`psoxide-d4d8006`.

## Source

This repository is the complete source of the emulator program that ships with Living Tool. It
is published from the `emu-host/` folder of Living Tool's own repository: each commit here holds
that folder exactly as a Living Tool build had it, and a Living Tool release only builds when the
program it ships is here. Tags `vX.Y.Z` mark the program's versions.

## Build

```sh
cargo build --release          # target/release/lt-emu-host
target/release/lt-emu-host --version
```

The toolchain is pinned in `rust-toolchain.toml`. Linux needs `libasound2-dev` (sound).

`hooks/examples/hooks_check.rs` checks the hooks on a Guardian's Crusade disc: a machine with
observe hooks against one without, compared after every frame, and a replace hook that drops the
field's terrain:

```sh
cargo run --release -p lt-emu-hooks --example hooks_check -- <disc.bin> <out dir> [identity|replace|all] [frames]
```

## As a library

`host/` is also the `lt_emu_host` library, for a program that hosts the game itself and speaks the
same protocol: `protocol_out()` takes stdout first thing (see below), and `serve(out, stepper,
events)` runs the whole server on the calling thread until `Shutdown` or a closed pipe. A
`Stepper` runs each frame on the core (`Plain`: the core's own loop, what this program uses), is
handed each new CPU and disc (`attach`: after `Open`, `Reset` and `LoadState`, which replace the
CPU), hears of `Patch`es, answers `Focus` and names the program in `Hello`. Living Tool Remaster
(`lt-remaster --host`) serves Living Tool's Play screen this way with code hooks and a native
renderer in its stepper, and its own window.

## The protocol

The application writes requests to the program's stdin and reads replies, events and pictures
from its stdout. Each message is a little-endian `u32` length, a kind byte, then the payload:
kind 0 is a [postcard](https://docs.rs/postcard) encoding of an `Envelope` (to the program) or an
`Event` (from it); kind 1 is a picture (a 32-byte header, then RGBA8 rows). Requests carry an id;
the reply carries the same id; id 0 wants no reply. `protocol/src/lib.rs` documents every message.

| Request | Does |
|---|---|
| `Hello` | Versions: the protocol, the core build, the program |
| `Open` | Boots a raw (2352-byte sector) image, with patched sectors over it and a memory card in slot 1 |
| `Patch` | Changes the patched sectors; the next CD read sees them |
| `Run`, `Pause`, `Reset` | Real time (paced by the emulated clock, with sound), stopped, booted again |
| `Pad`, `Press` | The pad held now; a press held for n frames |
| `Advance` | n frames as fast as possible, one pad each (scripted input, fast-forward) |
| `Batch` | RAM reads, guards and writes between two frames, all at once: writes happen only if every guard still matches |
| `SaveState`, `LoadState` | The machine as bytes (not the disc, not the card) |
| `Card`, `InsertCard` | The memory card as the game left it; another card |
| `Screenshot`, `Frames` | The picture now; every n-th frame as a picture while running |
| `Audio` | Volume and mute |
| `Diag` | Health counters |
| `Shutdown` | Hands back the memory card and exits |
| `Focus` | Shows the host's own window (protocol 2; this program has none and says so) |

Events: `Stats` once a second, `Card` when the game writes its memory card, `Halted` when the
core fails, `Log`, and `Closed` when the player closes a host's own window (protocol 2: the host
waits for `Shutdown`, so the application can take the machine back first). The program exits when
its stdin closes.

The core's HLE BIOS prints the game's TTY output to stdout; the program points that at stderr and
keeps its own copy of stdout for the protocol.
