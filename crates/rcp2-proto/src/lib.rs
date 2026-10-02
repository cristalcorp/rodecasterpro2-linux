//! Pure codec for the RØDECaster Pro II USB HID control protocol.
//!
//! This crate performs no I/O: it only turns bytes into typed values and back,
//! so it can be tested and fuzzed without hardware.
//!
//! Protocol facts come from the reverse-engineering notes of
//! [rodey](https://github.com/seanheiney/rodey/blob/main/docs/PROTOCOL.md) (MIT),
//! cross-checked against captures of our own device.
//!
//! # Hardware safety
//!
//! Some bytes on report 1 put the device into firmware update mode or trigger a
//! firmware flash. This crate is designed so that such frames cannot be built:
//! see [`ModeCommand`].

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
    pub const REPORT_ID: u8 = 1;

    /// Wire byte for this command.
    #[must_use]
    pub const fn byte(self) -> u8 {
        match self {
            Self::Normal => 0x4E,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ModeCommand;

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
}
