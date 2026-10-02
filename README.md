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
| `rcp2ctl` | Command-line tool, then TUI |

## Status

Bootstrapping. Nothing talks to the device yet.

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
