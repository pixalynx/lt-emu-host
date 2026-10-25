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
| `host/` | The program: PSoXide's core, the disc (a raw image with patched sectors over it), the real-time loop, sound out (cpal), the pipe | GPL-3.0-or-later ([LICENSE](LICENSE)) |
| `protocol/` | `lt-emu-protocol`: the messages and their framing, shared by the program and its applications | MIT ([protocol/LICENSE](protocol/LICENSE)) |

The core is PSoXide's `emulator-core` and `psx-iso`, GPL-2.0-or-later, pinned at
[`d4d8006`](https://github.com/EBonura/PSoXide-emulator/tree/d4d8006ebe3c945ed4a568ae7ef77e9fc0a5028e)
(2026-10-24). Machine states only load into the core build that saved them, so the rev only moves
deliberately.

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

Events: `Stats` once a second, `Card` when the game writes its memory card, `Halted` when the
core fails, `Log`. The program exits when its stdin closes.

The core's HLE BIOS prints the game's TTY output to stdout; the program points that at stderr and
keeps its own copy of stdout for the protocol.
