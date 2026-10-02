//! PipeWire side of the RØDECaster Pro II.
//!
//! The board already works as a USB audio interface through `snd-usb-audio`;
//! in the `pro-audio` profile it shows up as a 2-channel sink (Chat) and a
//! 10-channel sink carrying five stereo channels (USB1, Game, Music, A, B).
//! This crate:
//!
//! - detects the board and its native sinks in the PipeWire graph ([`Graph`]);
//! - generates a PipeWire configuration declaring one named stereo sink per
//!   channel ([`pipewire_config`]);
//! - lists application streams and moves them between sinks ([`move_stream`]).
//!
//! It drives PipeWire through its command-line tools (`pw-dump`,
//! `pw-metadata`) rather than through FFI, so it contains no `unsafe` code and
//! needs no C toolchain.

mod channel;
mod config;
mod graph;
mod pw;

pub use channel::{Channel, NativeSink, UnknownChannel};
pub use config::{CONFIG_FILE_NAME, UnsafeNodeName, pipewire_config};
pub use graph::{AppStream, DetectError, Graph, Link, Node, ParseError, Rode};
pub use pw::{InstallOutcome, PwError, config_path, install_config, move_stream, snapshot};
