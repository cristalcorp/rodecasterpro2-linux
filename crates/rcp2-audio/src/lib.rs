//! PipeWire side of the RØDECaster Pro II.
//!
//! The board already works as a USB audio interface through `snd-usb-audio`;
//! in the `pro-audio` profile it shows up as a 2-channel sink (Chat) and a
//! 10-channel sink carrying five stereo channels (USB1, Game, Music, A, B).
//! This crate:
//!
//! - detects the board and its native sinks in the PipeWire graph ([`Graph`]);
//! - creates one named stereo sink per channel, either at runtime with no file
//!   written ([`create_runtime_outputs`]), or persistently through a PipeWire
//!   config file that can be removed to restore the original state exactly
//!   ([`enable_persistence`], [`disable_persistence`]);
//! - lists application streams and moves them between sinks ([`move_stream`]).
//!
//! It drives PipeWire through its command-line tools (`pw-dump`,
//! `pw-metadata`, `pactl`) rather than through FFI, so it contains no `unsafe` code and
//! needs no C toolchain.

mod channel;
mod config;
mod graph;
mod persist;
mod pw;
mod runtime;

pub use channel::{Channel, NativeSink, UnknownChannel};
pub use config::{CONFIG_FILE_NAME, GENERATED_MARKER, UnsafeNodeName, pipewire_config};
pub use graph::{AppStream, DetectError, Graph, Node, ParseError, Rode};
pub use persist::{
    APP_DIR, DisableOutcome, EnableOutcome, PersistPaths, PersistState, config_home, data_home,
    disable_persistence, enable_persistence, original_recorded, persist_state,
    remove_file_if_present, write_atomically,
};
pub use pw::{PwError, move_stream, snapshot};
pub use runtime::{create_runtime_outputs, remap_sink_args, remove_runtime_outputs};
