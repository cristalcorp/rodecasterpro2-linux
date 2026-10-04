//! Pure codec for the RØDECaster Pro II USB HID control protocol.
//!
//! This crate performs no I/O: it only turns bytes into typed values and back,
//! so it can be tested and fuzzed without hardware.
//!
//! Protocol facts come from the reverse-engineering notes of
//! [rodey](https://github.com/seanheiney/rodey/blob/main/docs/PROTOCOL.md) (MIT)
//! and from [protocol findings](https://github.com/parzival-space/rodecaster-utility/issues/11)
//! published by the AccessCaster project (facts only, no code), cross-checked
//! against captures of our own device.
//!
//! # Hardware safety
//!
//! Some bytes on report 1 put the device into firmware update mode or trigger a
//! firmware flash. This crate is designed so that such frames cannot be built:
//! the only report-1 frame it can produce comes from [`ModeCommand`], which
//! has a single variant.

mod state;
mod stream;
mod tree;

pub use state::{BoardState, ChannelState, InputSource};
pub use stream::{
    ACK_REPORT_ID, Change, DumpAssembler, DumpError, Incoming, PathNotFound, apply_property,
    classify, decode_change,
};
pub use tree::{DecodeError, MAX_DEPTH, Node, Var, decode_tree, encode_tree};

/// USB vendor ID of RØDE Microphones.
pub const VENDOR_ID: u16 = 0x19F7;

/// USB product IDs of the RØDECaster Pro II this project has been verified
/// on. Others exist (other modes or firmware versions) but are not trusted
/// until checked on hardware.
pub const KNOWN_PRODUCT_IDS: &[u16] = &[0x0078];

/// Report ID carrying mode commands (host to device), 63 data bytes.
pub const MODE_REPORT_ID: u8 = 1;
/// Report ID carrying property writes and session control (host to device),
/// 255 data bytes.
pub const PROPERTY_OUT_REPORT_ID: u8 = 3;
/// Report ID carrying notifications and the state dump (device to host),
/// 255 data bytes.
pub const PROPERTY_IN_REPORT_ID: u8 = 4;

/// Data bytes of a mode report, from the HID report descriptor.
pub const MODE_REPORT_DATA_LEN: usize = 63;
/// Data bytes of a property report, from the HID report descriptor.
pub const PROPERTY_REPORT_DATA_LEN: usize = 255;

/// Session-open command: subscribes to notifications and triggers the dump.
const SESSION_OPEN: [u8; 4] = [0xAD, 0x10, 0xA7, 0xB0];

/// Single-byte mode command sent on HID report 1.
///
/// Only the normal/app mode is representable on purpose. Other known values
/// (`0x4D` enters firmware update mode, `0x55` triggers a firmware flash) must
/// never be emitted by this project, so they have no variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModeCommand {
    /// Normal / app mode (`'N'`, `0x4E`).
    Normal,
}

impl ModeCommand {
    /// HID report ID carrying mode commands.
    pub const REPORT_ID: u8 = MODE_REPORT_ID;

    /// Wire byte for this command.
    #[must_use]
    pub const fn byte(self) -> u8 {
        match self {
            Self::Normal => 0x4E,
        }
    }

    /// The full report to write to `hidraw`: report ID, command byte, zero
    /// padding up to the size declared by the report descriptor.
    #[must_use]
    pub const fn report(self) -> [u8; 1 + MODE_REPORT_DATA_LEN] {
        let mut report = [0; 1 + MODE_REPORT_DATA_LEN];
        report[0] = MODE_REPORT_ID;
        report[1] = self.byte();
        report
    }
}

/// The session-open report (second half of the handshake): report 3 carrying
/// a length-prefixed body `ad 10 a7 b0`, zero padded.
#[must_use]
pub const fn session_open_report() -> [u8; 1 + PROPERTY_REPORT_DATA_LEN] {
    let mut report = [0; 1 + PROPERTY_REPORT_DATA_LEN];
    report[0] = PROPERTY_OUT_REPORT_ID;
    // Body length, u32 little-endian.
    report[1] = 4;
    report[5] = SESSION_OPEN[0];
    report[6] = SESSION_OPEN[1];
    report[7] = SESSION_OPEN[2];
    report[8] = SESSION_OPEN[3];
    report
}

#[cfg(test)]
mod tests {
    use super::{
        MODE_REPORT_ID, ModeCommand, PROPERTY_OUT_REPORT_ID, PROPERTY_REPORT_DATA_LEN,
        session_open_report,
    };

    #[test]
    fn normal_mode_is_ascii_n() {
        assert_eq!(ModeCommand::Normal.byte(), b'N');
    }

    #[test]
    fn no_dangerous_mode_byte_is_emitted() {
        // 'M' (firmware update mode) and 'U' (firmware flash) must stay unreachable.
        assert_ne!(ModeCommand::Normal.byte(), b'M');
        assert_ne!(ModeCommand::Normal.byte(), b'U');
    }

    #[test]
    fn the_mode_report_carries_only_the_normal_byte() {
        let report = ModeCommand::Normal.report();
        assert_eq!(report.len(), 64);
        assert_eq!(report[..2], [MODE_REPORT_ID, 0x4E]);
        assert!(report[2..].iter().all(|byte| *byte == 0));
    }

    #[test]
    fn the_session_open_report_matches_the_documented_bytes() {
        let report = session_open_report();
        assert_eq!(report.len(), 1 + PROPERTY_REPORT_DATA_LEN);
        assert_eq!(
            report[..9],
            [
                PROPERTY_OUT_REPORT_ID,
                0x04,
                0x00,
                0x00,
                0x00,
                0xAD,
                0x10,
                0xA7,
                0xB0
            ]
        );
        assert!(report[9..].iter().all(|byte| *byte == 0));
    }
}
