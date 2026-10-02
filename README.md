# rodecasterpro2-linux

Linux control tool for the **RØDECaster Pro II**, written in Rust.

> **Unofficial.** Not affiliated with, endorsed by, or supported by RØDE Microphones.
> The control protocol is reverse-engineered and may break with any firmware update.
> **Experimental: use at your own risk.**

## Why

On Linux the RØDECaster Pro II already works as a USB audio interface
(`snd-usb-audio`), but nothing replaces RØDE Central / the RØDECaster app for
everything else. This project aims to:

1. **Expose the board's USB outputs as named virtual devices** (e.g. *Game*,
   *Music*, *Chat*) in PipeWire, and route each application to one of them.
2. **Read and drive the board's faders and pots** for each input/output over the
   vendor HID interface.
3. Provide a **TUI** first, a GUI maybe later.

## Design principles

- **Userspace only.** Audio stays in the kernel (`snd-usb-audio`); control goes
  through `hidraw`. No kernel module.
- **Hardware safety first.** Frames known to put the device into firmware update
  mode or to flash it are not representable in the code. Writes are only ever sent
  to object IDs observed in the device state, never discovered by sweeping.
- **Strict Rust.** `unsafe_code = "forbid"` in our crates, clippy `pedantic` as
  errors, no `unwrap`/`expect`/`panic` outside tests, `cargo-deny` in CI.

## Workspace

| Crate | Role |
|---|---|
| `rcp2-proto` | Pure codec for the HID control protocol (no I/O, fuzzable) |
| `rcp2-audio` | PipeWire side: board detection, named outputs, app routing |
| `rcp2ctl` | Command-line tool, then TUI |

## Status

Named outputs and app routing work (PipeWire side). Nothing talks to the
board's HID control interface yet.

## Usage

Requirements: PipeWire with WirePlumber, and the board in the **Pro Audio**
profile (pavucontrol, *Configuration* tab).

```sh
cargo build --release
target/release/rcp2ctl status              # board + named outputs
target/release/rcp2ctl config --install    # declare the outputs (backs up any previous file)
systemctl --user restart pipewire pipewire-pulse wireplumber
target/release/rcp2ctl apps                # who plays where
target/release/rcp2ctl route firefox game  # remembered for the app's next runs
```

The board's USB playback channels, as verified on hardware (firmware-dependent):

| Output | Native sink | Channels |
|---|---|---|
| RØDE Chat | `pro-output-0` (2 ch) | AUX0–AUX1 |
| RØDE USB1 | `pro-output-1` (10 ch) | AUX0–AUX1 |
| RØDE Game | `pro-output-1` | AUX2–AUX3 |
| RØDE Music | `pro-output-1` | AUX4–AUX5 |
| RØDE A | `pro-output-1` | AUX6–AUX7 |
| RØDE B | `pro-output-1` | AUX8–AUX9 |

Outputs are named after the board's channels, not its faders: any channel can be
assigned to any fader on the board.

To undo: delete `~/.config/pipewire/pipewire.conf.d/50-rodecaster-virtual-sinks.conf`
and restart PipeWire as above.

## Prior art and credits

- [seanheiney/rodey](https://github.com/seanheiney/rodey): the reference write-up of
  the HID protocol ([PROTOCOL.md](https://github.com/seanheiney/rodey/blob/main/docs/PROTOCOL.md)).
- Other community projects: [x1h0/rcp2-cli](https://github.com/x1h0/rcp2-cli),
  [Holfz/rodecaster-routing](https://github.com/Holfz/rodecaster-routing),
  [parzival-space/rodecaster-utility](https://github.com/parzival-space/rodecaster-utility),
  [Jordan-Milner/rodecaster-pro2-pipewire](https://github.com/Jordan-Milner/rodecaster-pro2-pipewire).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
