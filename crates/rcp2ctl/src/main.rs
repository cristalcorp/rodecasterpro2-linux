//! `rcp2ctl`: Linux control tool for the RØDECaster Pro II.
//!
//! Placeholder entry point: device discovery, the PipeWire routing view and the
//! TUI land in follow-up changes.

use std::io::Write as _;
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut out = std::io::stdout().lock();
    match writeln!(out, "rcp2ctl {}", env!("CARGO_PKG_VERSION")) {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::FAILURE,
    }
}
