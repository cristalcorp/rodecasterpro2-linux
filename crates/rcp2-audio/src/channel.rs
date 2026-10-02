//! The board's USB playback channels and how they map onto the native ALSA sinks.

use std::fmt;
use std::str::FromStr;

/// Native PipeWire sink exposed by the board in the `pro-audio` profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NativeSink {
    /// `pro-output-0`: 2 channels.
    Stereo,
    /// `pro-output-1`: 10 channels.
    Multi,
}

impl NativeSink {
    /// `device.profile.name` of this sink.
    #[must_use]
    pub const fn profile_name(self) -> &'static str {
        match self {
            Self::Stereo => "pro-output-0",
            Self::Multi => "pro-output-1",
        }
    }

    /// Number of channels the sink must have for the mapping to be valid.
    #[must_use]
    pub const fn channel_count(self) -> u64 {
        match self {
            Self::Stereo => 2,
            Self::Multi => 10,
        }
    }
}

/// A USB playback channel of the board, named as the board names it.
///
/// Channels are named by source, not by fader: the board lets the user assign
/// any channel to any fader. The mapping was verified on hardware by playing a
/// tone on each channel pair and reading the board's meters.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Channel {
    /// Chat (the stereo sink).
    Chat,
    /// USB 1 (multi sink, `AUX0`/`AUX1`).
    Usb1,
    /// Game (multi sink, `AUX2`/`AUX3`).
    Game,
    /// Music (multi sink, `AUX4`/`AUX5`).
    Music,
    /// Virtual A (multi sink, `AUX6`/`AUX7`).
    A,
    /// Virtual B (multi sink, `AUX8`/`AUX9`).
    B,
}

impl Channel {
    /// Every channel, in board order.
    pub const ALL: [Self; 6] = [
        Self::Chat,
        Self::Usb1,
        Self::Game,
        Self::Music,
        Self::A,
        Self::B,
    ];

    /// Short lowercase identifier, used on the command line and in node names.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Usb1 => "usb1",
            Self::Game => "game",
            Self::Music => "music",
            Self::A => "a",
            Self::B => "b",
        }
    }

    /// Name as shown on the board.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Chat => "Chat",
            Self::Usb1 => "USB1",
            Self::Game => "Game",
            Self::Music => "Music",
            Self::A => "A",
            Self::B => "B",
        }
    }

    /// Native sink carrying this channel.
    #[must_use]
    pub const fn native_sink(self) -> NativeSink {
        match self {
            Self::Chat => NativeSink::Stereo,
            Self::Usb1 | Self::Game | Self::Music | Self::A | Self::B => NativeSink::Multi,
        }
    }

    /// Left and right channel positions on the native sink.
    #[must_use]
    pub const fn positions(self) -> [&'static str; 2] {
        match self {
            Self::Chat | Self::Usb1 => ["AUX0", "AUX1"],
            Self::Game => ["AUX2", "AUX3"],
            Self::Music => ["AUX4", "AUX5"],
            Self::A => ["AUX6", "AUX7"],
            Self::B => ["AUX8", "AUX9"],
        }
    }

    /// `node.name` of the named virtual sink for this channel.
    #[must_use]
    pub fn sink_name(self) -> String {
        format!("rcp2.{}", self.id())
    }

    /// `node.description` of the named virtual sink, as shown in desktop mixers.
    #[must_use]
    pub fn description(self) -> String {
        format!("RØDE {}", self.label())
    }
}

impl fmt::Display for Channel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// Error returned when parsing an unknown channel name.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown channel `{0}` (expected one of: chat, usb1, game, music, a, b)")]
pub struct UnknownChannel(pub String);

impl FromStr for Channel {
    type Err = UnknownChannel;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|channel| channel.id().eq_ignore_ascii_case(s))
            .ok_or_else(|| UnknownChannel(s.to_owned()))
    }
}

#[cfg(test)]
mod tests {
    use super::{Channel, NativeSink};
    use std::collections::HashSet;

    #[test]
    fn parses_ids_case_insensitively() {
        assert_eq!("Music".parse::<Channel>().unwrap(), Channel::Music);
        assert_eq!("USB1".parse::<Channel>().unwrap(), Channel::Usb1);
        assert!("fader5".parse::<Channel>().is_err());
    }

    #[test]
    fn every_id_round_trips() {
        for channel in Channel::ALL {
            assert_eq!(channel.id().parse::<Channel>().unwrap(), channel);
        }
    }

    #[test]
    fn no_two_channels_share_a_native_pair() {
        let pairs: HashSet<_> = Channel::ALL
            .into_iter()
            .map(|channel| (channel.native_sink(), channel.positions()))
            .collect();
        assert_eq!(pairs.len(), Channel::ALL.len());
    }

    #[test]
    fn multi_sink_pairs_cover_all_ten_channels() {
        let mut positions: Vec<_> = Channel::ALL
            .into_iter()
            .filter(|channel| channel.native_sink() == NativeSink::Multi)
            .flat_map(Channel::positions)
            .collect();
        positions.sort_unstable();
        let expected: Vec<String> = (0..10).map(|i| format!("AUX{i}")).collect();
        assert_eq!(positions, expected);
    }
}
