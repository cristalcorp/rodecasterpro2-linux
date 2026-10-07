//! The parts of the board's tree this project uses, as plain values.

use crate::tree::{Node, Var};

/// What feeds a channel strip (`channelInputSource`).
///
/// Only the codes verified on a real board are named; the others are kept as
/// raw codes rather than guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputSource {
    /// Code `0`: microphone input 1 (verified).
    Mic1,
    /// Code `7`: USB 1 (verified).
    Usb1,
    /// Code `8`: Chat (verified).
    Chat,
    /// Code `11`: SMART Pads (verified).
    SmartPads,
    /// Code `12`: Game, a virtual USB channel (verified).
    Game,
    /// Code `13`: Music, a virtual USB channel (verified).
    Music,
    /// Code `-1`: nothing assigned.
    Empty,
    /// No readable `channelInputSource` (missing or not an integer).
    Unknown,
    /// Any other code, not verified yet.
    Code(i32),
}

impl InputSource {
    /// The named sources; their codes are written once, in [`Self::code`].
    const NAMED: [Self; 7] = [
        Self::Mic1,
        Self::Usb1,
        Self::Chat,
        Self::SmartPads,
        Self::Game,
        Self::Music,
        Self::Empty,
    ];

    /// Interprets a `channelInputSource` value.
    #[must_use]
    pub fn from_code(code: i32) -> Self {
        Self::NAMED
            .into_iter()
            .find(|named| named.code() == Some(code))
            .unwrap_or(Self::Code(code))
    }

    /// The `channelInputSource` code, `None` for [`InputSource::Unknown`].
    #[must_use]
    pub const fn code(self) -> Option<i32> {
        // The one table of codes: a named source and its code.
        match self {
            Self::Mic1 => Some(0),
            Self::Usb1 => Some(7),
            Self::Chat => Some(8),
            Self::SmartPads => Some(11),
            Self::Game => Some(12),
            Self::Music => Some(13),
            Self::Empty => Some(-1),
            Self::Code(code) => Some(code),
            Self::Unknown => None,
        }
    }
}

impl std::fmt::Display for InputSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Mic1 => f.write_str("Mic 1"),
            Self::Usb1 => f.write_str("USB 1"),
            Self::Chat => f.write_str("Chat"),
            Self::SmartPads => f.write_str("SMART Pads"),
            Self::Game => f.write_str("Game"),
            Self::Music => f.write_str("Music"),
            Self::Empty => f.write_str("(empty)"),
            Self::Unknown => f.write_str("?"),
            Self::Code(code) => write!(f, "source {code}"),
        }
    }
}

/// One channel strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelState {
    /// Index of the `CHANNEL` node under the root: its address for changes.
    pub index: usize,
    /// What feeds it.
    pub source: InputSource,
    /// Whether its output is muted (`channelOutputMute`), if reported.
    pub muted: Option<bool>,
}

/// The board state as used by this project.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BoardState {
    /// `SYSTEM.systemFirmwareVersion`.
    pub firmware: Option<String>,
    /// Channel strips, in tree order.
    pub channels: Vec<ChannelState>,
    /// Fader positions (`PHYSICALINTERFACE/FADER.faderLevel`, 0–127), one
    /// entry per fader in order; `None` if a level is unreadable.
    pub faders: Vec<Option<i32>>,
}

impl BoardState {
    /// Extracts the state from a full tree (the root node of a dump).
    #[must_use]
    pub fn from_tree(root: &Node) -> Self {
        let firmware = root.children_named("SYSTEM").find_map(|(_, system)| {
            match system.property("systemFirmwareVersion") {
                Some(Var::String(version)) => Some(version.clone()),
                _ => None,
            }
        });
        let channels = root
            .children_named("CHANNEL")
            .map(|(index, channel)| ChannelState {
                index,
                source: match channel.property("channelInputSource") {
                    Some(Var::Int(code)) => InputSource::from_code(*code),
                    _ => InputSource::Unknown,
                },
                muted: match channel.property("channelOutputMute") {
                    Some(Var::Bool(muted)) => Some(*muted),
                    _ => None,
                },
            })
            .collect();
        let faders = root
            .children_named("PHYSICALINTERFACE")
            .flat_map(|(_, panel)| panel.children_named("FADER"))
            // One entry per fader, readable or not, so positions never shift.
            .map(|(_, fader)| match fader.property("faderLevel") {
                Some(Var::Int(level)) => Some(*level),
                _ => None,
            })
            .collect();
        Self {
            firmware,
            channels,
            faders,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BoardState, ChannelState, InputSource};
    use crate::tree::{Node, Var};

    fn node(name: &str, properties: Vec<(&str, Var)>, children: Vec<Node>) -> Node {
        Node {
            name: name.to_owned(),
            properties: properties
                .into_iter()
                .map(|(key, value)| (key.to_owned(), value))
                .collect(),
            children,
        }
    }

    fn channel(source: i32, muted: bool) -> Node {
        node(
            "CHANNEL",
            vec![
                ("channelInputSource", Var::Int(source)),
                ("channelOutputMute", Var::Bool(muted)),
            ],
            vec![],
        )
    }

    #[test]
    fn extracts_firmware_channels_and_faders() {
        let fader = |level| node("FADER", vec![("faderLevel", Var::Int(level))], vec![]);
        let root = node(
            "Rodecaster",
            vec![],
            vec![
                node(
                    "PHYSICALINTERFACE",
                    vec![],
                    vec![fader(45), node("POT", vec![], vec![]), fader(26)],
                ),
                node(
                    "SYSTEM",
                    vec![("systemFirmwareVersion", Var::String("1.7.6".to_owned()))],
                    vec![],
                ),
                channel(0, true),
                channel(7, false),
                channel(-1, false),
                channel(12, false),
            ],
        );
        let state = BoardState::from_tree(&root);
        assert_eq!(state.firmware.as_deref(), Some("1.7.6"));
        assert_eq!(state.faders, [Some(45), Some(26)]);
        assert_eq!(
            state.channels,
            [
                ChannelState {
                    index: 2,
                    source: InputSource::Mic1,
                    muted: Some(true)
                },
                ChannelState {
                    index: 3,
                    source: InputSource::Usb1,
                    muted: Some(false)
                },
                ChannelState {
                    index: 4,
                    source: InputSource::Empty,
                    muted: Some(false)
                },
                ChannelState {
                    index: 5,
                    source: InputSource::Game,
                    muted: Some(false)
                },
            ]
        );
    }

    #[test]
    fn unreadable_values_keep_their_place() {
        let root = node(
            "Rodecaster",
            vec![],
            vec![
                node(
                    "PHYSICALINTERFACE",
                    vec![],
                    vec![
                        node("FADER", vec![("faderLevel", Var::Double(0.5))], vec![]),
                        node("FADER", vec![("faderLevel", Var::Int(9))], vec![]),
                    ],
                ),
                node(
                    "CHANNEL",
                    vec![("channelInputSource", Var::Bool(true))],
                    vec![],
                ),
            ],
        );
        let state = BoardState::from_tree(&root);
        assert_eq!(state.faders, [None, Some(9)]);
        assert_eq!(state.channels[0].source, InputSource::Unknown);
        assert_eq!(InputSource::Unknown.code(), None);
        assert_eq!(InputSource::Game.code(), Some(12));
        for code in [-1, 0, 7, 8, 9, 11, 12, 13] {
            assert_eq!(InputSource::from_code(code).code(), Some(code));
        }
    }

    #[test]
    fn every_named_source_is_read_back_from_its_code() {
        let named = |source: InputSource| match source {
            InputSource::Mic1
            | InputSource::Usb1
            | InputSource::Chat
            | InputSource::SmartPads
            | InputSource::Game
            | InputSource::Music
            | InputSource::Empty => true,
            // Not named: a new variant must be sorted here, and listed in
            // `NAMED` if it is.
            InputSource::Unknown | InputSource::Code(_) => false,
        };
        assert!(InputSource::NAMED.into_iter().all(named));
        for source in InputSource::NAMED {
            let code = source.code().unwrap();
            assert_eq!(InputSource::from_code(code), source);
        }
        assert_eq!(InputSource::from_code(9), InputSource::Code(9));
        // No two named sources share a code.
        for (position, source) in InputSource::NAMED.into_iter().enumerate() {
            assert!(!InputSource::NAMED[position + 1..].contains(&source));
            assert!(
                InputSource::NAMED[position + 1..]
                    .iter()
                    .all(|other| other.code() != source.code())
            );
        }
    }

    #[test]
    fn an_empty_tree_gives_an_empty_state() {
        assert_eq!(
            BoardState::from_tree(&Node::default()),
            BoardState::default()
        );
    }
}
