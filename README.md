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
| `rcp2-audio` | PipeWire side: board detection, named outputs (runtime or persistent), app routing |
| `rcp2ctl` | Command-line tool, then TUI |

## Status

Named outputs and app routing work (PipeWire side). Nothing talks to the
board's HID control interface yet.

## Usage

Requirements: PipeWire with WirePlumber and `pipewire-pulse` (`pactl`), and the
board in the **Pro Audio** profile (pavucontrol, *Configuration* tab).

```sh
cargo build --release
target/release/rcp2ctl status              # board + named outputs (creates them if missing)
target/release/rcp2ctl apps                # who plays where
target/release/rcp2ctl route firefox game  # remembered for the app's next runs
```

### How the named outputs are kept

- **By default, no file is touched.** Every launch of `rcp2ctl` creates the
  missing outputs at runtime inside PipeWire. They last until PipeWire restarts
  (e.g. a reboot) and come back the next time `rcp2ctl` runs. If `rcp2ctl` is
  never launched, the system stays exactly as it was.
- **`rcp2ctl persist on`** keeps them across reboots without launching
  `rcp2ctl`, through a PipeWire config file. What was at that path before is
  recorded first; **`rcp2ctl persist off`** restores it byte for byte. No
  PipeWire restart is needed either way.
- **`rcp2ctl outputs off|on`** removes or brings back the runtime outputs.
- **`rcp2ctl uninstall`** undoes everything before you remove the binary. It
  asks whether to restore the original PipeWire configuration (default: yes).
  Removing the binary directly cannot ask anything, so run this first.

The application's own settings live in `~/.config/rodecasterpro2-linux/`, and
the record of the original state in `~/.local/share/rodecasterpro2-linux/`.

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
