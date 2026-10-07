# rodecasterpro2-linux

A Linux tool, written in Rust, for the **RØDECaster Pro II**: named audio
outputs ("RØDE Game", "RØDE Music"…), per-application routing, a terminal
interface, and — in progress — reading the board's own state.

> **Unofficial.** This project is not affiliated with, endorsed by, or supported
> by RØDE Microphones. The board's control protocol is reverse-engineered and may
> change with any firmware update. Read [Risks](#5-risks) before using the board
> control commands (`rcp2ctl hid …`).

## Contents

1. [What it does](#1-what-it-does)
2. [How it works](#2-how-it-works)
3. [Requirements](#3-requirements)
4. [Installation](#4-installation)
5. [Risks](#5-risks)
6. [First steps](#6-first-steps)
7. [The terminal interface (TUI)](#7-the-terminal-interface-tui)
8. [Command reference](#8-command-reference)
9. [How the named outputs are kept](#9-how-the-named-outputs-are-kept)
10. [Board control interface (HID)](#10-board-control-interface-hid)
11. [Files and settings touched on your system](#11-files-and-settings-touched-on-your-system)
12. [Uninstallation](#12-uninstallation)
13. [Troubleshooting](#13-troubleshooting)
14. [Status and roadmap](#14-status-and-roadmap)
15. [Development](#15-development)
16. [Prior art and credits](#16-prior-art-and-credits)
17. [License](#17-license)

## 1. What it does

On Linux the RØDECaster Pro II already works as a USB audio interface: the
kernel's standard driver handles sound in both directions. What Linux lacks is
everything RØDE's own apps do on Windows and macOS. This tool adds:

- **Named outputs.** The board's USB playback channels appear as six separate
  outputs — RØDE Chat, RØDE USB1, RØDE Game, RØDE Music, RØDE A and RØDE B —
  each landing on the matching channel of the board. Send your game to "RØDE
  Game" and your music player to "RØDE Music", and each one gets its own fader.
- **Routing.** Move any application to any of these outputs, from the terminal
  interface or the command line. The choice is remembered for the next time the
  application starts.
- **Default output.** Choose which output the system uses by default.
- **A terminal interface** that shows all of the above at a glance.
- **Board state (in progress).** Reading fader levels, mutes and channel
  assignments from the board itself.

## 2. How it works

- **Audio stays in the kernel.** The tool does not replace any driver. It works
  in userspace only, on top of PipeWire, the standard Linux sound server.
- **Named outputs** are small virtual outputs created inside PipeWire. Each one
  forwards a stereo signal, unchanged, to one pair of the board's USB channels.
- **The board's control interface** is a vendor-specific USB "HID" interface,
  separate from the audio. The tool reads it through the standard Linux
  `hidraw` device, without root once access is set up.
- **No unsafe code, no C library.** The tool drives PipeWire through its own
  command-line tools (`pw-dump`, `pw-metadata`, `pactl`, `wpctl`) and the board
  through plain file reads and writes.

The board's USB channels, as verified on hardware (they may differ with other
firmware versions):

| Output | Board channel | PipeWire sink | Channels |
|---|---|---|---|
| RØDE Chat | Chat | `pro-output-0` (2 ch) | AUX0–AUX1 |
| RØDE USB1 | USB 1 | `pro-output-1` (10 ch) | AUX0–AUX1 |
| RØDE Game | Game | `pro-output-1` | AUX2–AUX3 |
| RØDE Music | Music | `pro-output-1` | AUX4–AUX5 |
| RØDE A | Virtual A | `pro-output-1` | AUX6–AUX7 |
| RØDE B | Virtual B | `pro-output-1` | AUX8–AUX9 |

Outputs are named after the board's channels, not its faders: on the board,
any channel can be assigned to any fader.

## 3. Requirements

- Linux with **PipeWire** and **WirePlumber**, and the PulseAudio compatibility
  server **`pipewire-pulse`** (it provides `pactl`). These are the default on
  most current distributions.
- The board connected over USB, with the **Pro Audio** profile selected for it
  (for example in pavucontrol, *Configuration* tab).
- To build: a Rust toolchain (`rustup`); the exact version is pinned in
  `rust-toolchain.toml` and installed automatically.
- For board control only: `sudo`, once, to grant your user access to the board
  (see [Board access](#101-board-access)).

### Tested setup

Everything in this README was tested on this stack only (October 2026). Other
versions and distributions should work, but are not verified yet:

| Component | Version |
|---|---|
| Distribution | Arch Linux |
| Kernel (`snd-usb-audio`, `hidraw`) | 7.2.8 |
| PipeWire, with `pipewire-pulse`, `pipewire-alsa`, `pipewire-jack` | 1.6.9 |
| WirePlumber | 0.5.18 |
| PulseAudio daemon | none (`pactl` talks to `pipewire-pulse`) |
| systemd (user service) | 262 |
| RØDECaster Pro II firmware | 1.7.6 (state reading also checked on 1.6.8) |

When you report a problem, please give the same details: `uname -r`,
`pipewire --version`, `wireplumber --version`, your distribution, and the
board's firmware version (shown by `rcp2ctl board`).

## 4. Installation

There is no package yet. Build from source:

```sh
git clone https://github.com/cristalcorp/rodecasterpro2-linux.git
cd rodecasterpro2-linux
cargo build --release
```

The result is a single binary, `target/release/rcp2ctl`. Copy it anywhere on
your `PATH`, or install it into `~/.cargo/bin`:

```sh
cargo install --path crates/rcp2ctl --locked
```

Nothing else is installed. The tool changes your system only when you run it,
and only as described in [Files and settings touched](#11-files-and-settings-touched-on-your-system).

## 5. Risks

Read this before using the board control commands.

**Audio features (outputs, routing, default output, TUI).** These only talk to
PipeWire and never to the board's control interface. The worst case is that an
application plays on another output than expected; everything is undone by
`rcp2ctl uninstall`.

**Board control (`rcp2ctl hid …`).** The protocol is undocumented by RØDE and
was reverse-engineered by the community. The tool is designed so that the known
dangerous commands cannot be sent at all:

- The only mode command it can build is "normal mode". The bytes that put the
  board into firmware-update mode or start a firmware flash are not
  representable in the code.
- It never writes a setting to the board yet: today it only reads.
- It never updates, sends or touches firmware.

One effect is known and harmless, but you need to know about it:

> **Once a session is open, the board's faders stop controlling the volume as
> soon as nobody reads its control interface**, until you unplug and replug its
> USB cable. Nothing is damaged and no setting is lost. The cause: the board
> keeps sending notifications and freezes its controls when they are not read.
> On Windows and macOS the system always reads them; on Linux it stops when the
> program exits.
>
> The **board service** installed by `rcp2ctl hid setup` reads them for as long
> as it runs, so this does not happen in normal use. It does happen after a
> one-off `hid capture` without the service, and after the service is stopped
> (for example by `rcp2ctl uninstall`): replug the board, or start the service
> again (`systemctl --user restart rodecasterpro2-linux`), which also gives the
> faders back. **Do not run a capture during a recording or a live show.**
>
> **Turn the board off before the computer.** If the computer shuts down first,
> the service stops with it, the board freezes, and it may then hang when you
> turn it off: unplug it in that case.

## 6. First steps

1. Connect the board and select the **Pro Audio** profile for it.
2. Run `rcp2ctl`. The named outputs appear immediately; nothing is written to
   disk.
3. Start some music, select the player in the list, and press `4` to send it to
   RØDE Music.

## 7. The terminal interface (TUI)

Run `rcp2ctl` with no arguments. It needs a real terminal.

The screen has a header (board, how the outputs are kept, default output), an
**Outputs** panel on the left and an **Applications** panel on the right.

- ★ marks the system's default output.
- ♪ *n* shows how many applications play on an output.
- F*n* shows which fader of the board carries that output (needs the board
  service, see below).
- "absent" means the named output does not exist right now.

When the board service runs (`rcp2ctl hid setup`), a **Console** panel at the
bottom shows the board's channel strips: fader, source, whether the output is
muted (updated live) and the fader level. Fader levels are as of the last full
read of the board: the board sends no update when a fader moves. Without the
service, the panel says how to start it; everything else works the same.

| Key | Action |
|---|---|
| `↑` `↓` or `j` `k` | Move the selection |
| `Tab`, `←`, `→` | Switch between the two panels |
| `1` … `6` | Send the selected application to that output (Applications panel) |
| `Enter` or `d` | Make the selected output the system default (Outputs panel) |
| `o` | Named outputs on / off |
| `p` | Keep the outputs after a reboot (installs a PipeWire config file) |
| `r` | Restore the original PipeWire configuration |
| `F5` or `Ctrl+L` | Refresh now (the view also refreshes every second) |
| `?` | Help |
| `q`, `Esc` or `Ctrl+C` | Quit |

Every action shows its result on the message line, including how to undo it.

## 8. Command reference

Every command also accepts `--help`. All of them, except `status`, `config`,
the `hid` commands and `uninstall`, first recreate the named outputs if they are missing
(see [How the named outputs are kept](#9-how-the-named-outputs-are-kept)).

### `rcp2ctl`

Opens the terminal interface. Refuses to start without a terminal.

### `rcp2ctl status`

Shows the board's PipeWire sinks, whether each named output is present, and how
the outputs are kept (created at each launch, kept by the config file, or off).
Read-only: a missing output is shown as `absent`, never created. Also shows
whether the board service is installed and up to date. Works with the board
unplugged too: it says so and shows the rest.

### `rcp2ctl apps`

Lists the applications playing audio: stream ID, process binary, application
name, output and what is playing. Several applications can share a binary (Wine
games, Electron apps): use the stream ID to target exactly one.

### `rcp2ctl route <app> <output>`

Sends an application to a named output. `<app>` is a stream ID, a process
binary or an application name, exactly as listed by `apps` (case-insensitive,
no partial matches); every matching stream is moved. `<output>` is one of
`chat`, `usb1`, `game`, `music`, `a`, `b`. WirePlumber remembers the choice for
the application's next runs.

```sh
rcp2ctl route firefox game
rcp2ctl route 320 music
```

### `rcp2ctl default <output>`

Makes a named output the system's default output. WirePlumber remembers it.

### `rcp2ctl outputs on|off`

`on` creates the named outputs now and at every launch (the default). `off`
removes them; applications playing on them move to the default output. Refused
while persistence is on: turn persistence off first.

### `rcp2ctl persist on|off`

`on` keeps the named outputs after a reboot, even if `rcp2ctl` is never run
again, by installing a PipeWire config file. What was at that path before (a
file, or nothing) is recorded first. `off` restores exactly that original
state. No PipeWire restart is needed either way.

### `rcp2ctl config`

Prints the PipeWire configuration that `persist on` installs, without
installing anything. Useful for packagers.

### `rcp2ctl board`

Shows the board's own state — firmware version, each channel strip (source,
muted or not), fader positions (0–127) and how many changes were applied since
the last full read — as kept up to date by the board service. Fails with a
clear message if the service is not running.

### `rcp2ctl daemon`

Runs the board service in the foreground. You normally never run it yourself:
`hid setup` installs it as a systemd user service started at login. See
[The board service](#102-the-board-service).

### `rcp2ctl hid setup`

Grants your user access to the board's control interface by installing a udev
rule (asks for your `sudo` password), then installs and starts the board
service. Running it again updates both. See [Board access](#101-board-access).

### `rcp2ctl hid find`

Prints the board's `hidraw` device. Opens nothing.

### `rcp2ctl hid capture <file>.rcp2cap [--seconds N] [--yes]`

Opens a session with the board and records what it sends, to study the
protocol. Read-only. **Asks for confirmation** because of the fader freeze
described in [Risks](#5-risks); `--yes` skips the question. Stops after the
initial state dump, or after `N` seconds (1 to 60, default 10). If reading fails midway, the reports received so far are still saved; if the board cannot be reached, no file is left behind. The file must end in
`.rcp2cap` and is never overwritten. It contains the board's serial number:
keep it private (git ignores this extension).

### `rcp2ctl hid decode <file>.rcp2cap`

Decodes a capture offline — nothing is sent to the board — and shows the
firmware version, each channel strip (position in the board's state tree,
source, muted or not) and the fader positions (0–127). Sources not yet verified
on hardware are shown as a raw code. The serial number is never printed.

### `rcp2ctl uninstall [--yes | --keep-pipewire-config]`

Undoes everything the tool did. See [Uninstallation](#12-uninstallation).

## 9. How the named outputs are kept

- **By default, no file is written.** Every time `rcp2ctl` runs, it re-reads
  its own settings and creates any missing named outputs inside PipeWire. They
  last until PipeWire restarts (for example at a reboot), even after `rcp2ctl`
  exits. If `rcp2ctl` is never run again, or crashes, your system is exactly as
  it was after the next reboot.
- **Persistence (`persist on`, or `p` in the TUI)** keeps them across reboots
  through a PipeWire config file. Before writing it, the tool records what was
  at that path, with its permissions. **`persist off` (or `r`) puts it back
  byte for byte**, even if the generated file was deleted by hand in between.
- **When the board is unplugged or reset**, its sound card goes away and the
  named outputs remove themselves with it (rather than sending their sound to
  another device). The board service recreates them as soon as the board is
  back, if they are on; without the service, run `rcp2ctl` once.
- **Files the tool did not write are never overwritten**, and symbolic links
  (dotfiles managers, Nix) are never followed or replaced: the tool stops and
  tells you.

## 10. Board control interface (HID)

### 10.1 Board access

The board's control interface (`/dev/hidrawN`) is readable only by root by
default. `rcp2ctl hid setup` installs a udev rule, embedded in the binary, that
gives the **logged-in user** access to this one device, then applies it at
once (no replug). It does nothing if the rule is already installed and never
replaces a file it did not write. `rcp2ctl uninstall` removes it.

Distribution packages should ship `packaging/udev/70-rodecaster-pro-2.rules` in
`/usr/lib/udev/rules.d/` instead.

### 10.2 The board service

The service (`rcp2ctl daemon`, run by systemd as your user, never as root)
opens the session with the board, keeps reading it so that the faders keep
working, and keeps a copy of the board's state up to date from its
notifications. When the board is unplugged it waits and reconnects on its own,
and recreates the named outputs once the board's sound card is back (see
[How the named outputs are kept](#9-how-the-named-outputs-are-kept)).

Other commands ask it for the state through a socket in your private runtime
directory (`$XDG_RUNTIME_DIR/rodecasterpro2-linux/board.sock`, mode 0600):
only your user can talk to it. It is read-only: it never writes a setting to
the board.

Only one service runs at a time: it holds a lock file next to the socket, and a
second one refuses to start. If the service crashes, systemd starts it again
after 2 seconds; meanwhile, and while it reads the board again, the TUI keeps
showing the last state, marked "refreshing".

It runs the binary that installed it: if you move or rebuild `rcp2ctl`
elsewhere, or after upgrading, run `rcp2ctl hid setup` again. `rcp2ctl status`
tells you when the installed service was written by an older version.

### 10.3 What is read

The board answers a session request with a dump of its whole state (about
90 KB): channels and their sources, faders, mutes, processing, pads, system
settings. The tool decodes it exactly (it is a JUCE `ValueTree`). Verified on
firmware 1.6.8: fader positions and mute states match the board.

### 10.4 What is never done

- Sending any mode byte other than "normal mode".
- Writing settings that were not read from the board first, or guessing setting
  addresses.
- Writing fader levels: the faders are physical and not motorised; the board
  refuses it by design.
- Anything related to firmware updates.

## 11. Files and settings touched on your system

| What | When | Undone by |
|---|---|---|
| Named outputs inside PipeWire (in memory, no file) | Every run, if outputs are on | `outputs off`, a PipeWire restart, `uninstall` |
| `~/.config/rodecasterpro2-linux/config.toml` (the tool's settings) | `outputs on/off` | `uninstall` |
| `~/.config/pipewire/pipewire.conf.d/50-rodecaster-virtual-sinks.conf` | `persist on` | `persist off`, `uninstall` |
| `~/.local/share/rodecasterpro2-linux/original/` (record of the original) | `persist on` | `persist off`, `uninstall` |
| Per-application output memory, default output (WirePlumber state) | `route`, `default` | your desktop's sound settings |
| `/etc/udev/rules.d/70-rodecaster-pro-2.rules` | `hid setup` | `uninstall` |
| `~/.config/systemd/user/rodecasterpro2-linux.service` (board service) | `hid setup` | `uninstall` |
| `$XDG_RUNTIME_DIR/rodecasterpro2-linux/board.sock` (in memory) | while the service runs | replaced at the next start, logout |
| `$XDG_RUNTIME_DIR/rodecasterpro2-linux/board.lock` (in memory, empty) | while the service runs | logout |
| `$XDG_RUNTIME_DIR/rodecasterpro2-linux/outputs.lock` (in memory, empty) | any command creating or removing outputs | logout |
| Your `.rcp2cap` capture files | `hid capture` | delete them yourself |

## 12. Uninstallation

Run this **before** removing the binary; removing the binary alone cannot undo
anything.

```sh
rcp2ctl uninstall
```

It:

1. Asks **"Restore the original PipeWire configuration?"** if persistence was
   on. The default answer is yes (also without a terminal). `--yes` restores
   without asking; `--keep-pipewire-config` keeps the file without asking.
2. Stops and removes the board service. **Unplug and replug the board's USB
   cable afterwards**, so that its faders work again.
3. Removes the board access rule, if `hid setup` installed it (asks for your
   `sudo` password).
4. Removes the named outputs created at runtime.
5. Removes the tool's settings.
6. Tells you where the binary is, so you can delete it.

Every step runs even if a previous one fails; problems are listed and the
command exits with an error. Files changed by hand and symbolic links are left
in place, with a message.

Then delete the binary (`cargo uninstall rcp2ctl` if installed with
`cargo install`). WirePlumber's per-application memory can be reset from your
desktop's sound settings.

## 13. Troubleshooting

**"no RØDECaster Pro II found"** — Check the USB cable, that the board is on,
and that `pactl list short sinks` shows it.

**"has no `pro-output-1` sink"** — Select the **Pro Audio** profile for the
board (pavucontrol, *Configuration* tab).

**The named outputs are missing after a reboot** — Expected without
persistence: run `rcp2ctl` once, or turn persistence on.

**The faders no longer change the volume** — A session was opened and nobody
reads the board any more: a capture was run without the service, or the
service was stopped (see [Risks](#5-risks)). Start the service again
(`systemctl --user restart rodecasterpro2-linux`) or unplug and replug the
board's USB cable; with the service running, it does not happen again. A USB
reset (`usbreset`) does not help.

**The board hangs when you turn it off** — The computer was shut down first,
so nothing read the board any more. Unplug it; next time, turn the board off
before the computer.

**An application shows it is playing but nothing comes out, after PipeWire
was restarted** — Some applications do not reconnect to PipeWire on their own
(seen with qbz): quit and start the application again. WirePlumber still
remembers its output.

**The named outputs disappeared after unplugging the board** — The board
service recreates them when it is back. Without the service, run `rcp2ctl`
once.

**"the board service is not running"** — Run `rcp2ctl hid setup`. To see why
it stopped: `journalctl --user -u rodecasterpro2-linux`.

**"Permission denied" on `/dev/hidrawN`** — Run `rcp2ctl hid setup`.

**"… is a symlink" or "… was changed outside rcp2ctl"** — The tool refuses to
touch a file it does not own. Review the file, then move it away and rerun.

**"the interactive interface needs a terminal"** — `rcp2ctl` without arguments
opens the TUI; in scripts, use the commands from the reference.

## 14. Status and roadmap

| Area | State |
|---|---|
| Named outputs, routing, default output | Done |
| Terminal interface | Done (audio side) |
| Reading the board's state | Done: exact decoder, board service, `rcp2ctl board` |
| Fix for the fader freeze | Done while the board service runs |
| Board state in the TUI | Done: Console panel, fader of each output |
| Live fader levels in the TUI | Planned (faders send no live notification; MIDI is being evaluated) |
| Writing board settings (mutes, gain, processing) | Later, one setting type at a time, each tested on hardware first |

## 15. Development

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

- Workspace: `rcp2-proto` (pure protocol codec, no I/O), `rcp2-hid` (`hidraw`
  transport), `rcp2-audio` (PipeWire side), `rcp2ctl` (TUI and CLI).
- Strict rules: `unsafe` forbidden, clippy `pedantic` as errors, no
  `unwrap`/`expect`/`panic` outside tests, `cargo-deny` and secret scanning in
  CI, protected `main`, every change through a reviewed pull request.
- The decoder is tested without hardware, on anonymised fixtures. Captures of
  real boards contain serial numbers and never go into the repository.

## 16. Prior art and credits

- [seanheiney/rodey](https://github.com/seanheiney/rodey) (MIT): the reference
  write-up of the HID protocol.
- [AccessCaster's protocol findings](https://github.com/parzival-space/rodecaster-utility/issues/11)
  (facts only; no code from GPL projects is used).
- Other community projects: [x1h0/rcp2-cli](https://github.com/x1h0/rcp2-cli),
  [Holfz/rodecaster-routing](https://github.com/Holfz/rodecaster-routing),
  [parzival-space/rodecaster-utility](https://github.com/parzival-space/rodecaster-utility),
  [Jordan-Milner/rodecaster-pro2-pipewire](https://github.com/Jordan-Milner/rodecaster-pro2-pipewire).

## 17. License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT), at your option.
