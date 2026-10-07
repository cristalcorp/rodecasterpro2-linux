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
    /// Its level (0–127), as set by its fader; `None` for an empty or
    /// unknown source, or if the level is unreadable.
    pub level: Option<i32>,
}

/// Mix buses per source: the root holds one `MIX` node per source and bus,
/// source by source, in code order (390 = 30 × 13 on firmware 1.7.6).
const MIX_BUSES: usize = 13;

/// The level of the source with `code`, read from its first `MIX` node.
///
/// The `MIX` nodes are counted in tree order, not addressed by index: from
/// code 12 on, other nodes sit between them (I-013). `mixLevelWithAnchor` is
/// `"<anchor>|<level>"`, the level from 0 to 1, the same on every bus of a
/// source; it follows the fader live.
fn mix_level(mixes: &[&Node], code: i32) -> Option<i32> {
    let source = usize::try_from(code).ok()?;
    let Some(Var::String(value)) = mixes
        .get(source.checked_mul(MIX_BUSES)?)?
        .property("mixLevelWithAnchor")
    else {
        return None;
    };
    let (_, level) = value.split_once('|')?;
    let level: f64 = level.parse().ok()?;
    // Also rejects NaN.
    if !(0.0..=1.0).contains(&level) {
        return None;
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "between 0 and 127 after the range check"
    )]
    Some((level * 127.0).round() as i32)
}

/// The board state as used by this project.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BoardState {
    /// `SYSTEM.systemFirmwareVersion`.
    pub firmware: Option<String>,
    /// Channel strips, in tree order.
    pub channels: Vec<ChannelState>,
    /// Number of physical faders (`PHYSICALINTERFACE/FADER` nodes): strip
    /// `n` sits on fader `n + 1`. Their `faderLevel` is not the fader's
    /// position (I-013) and is not read.
    pub fader_count: usize,
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
        let mixes: Vec<&Node> = root.children_named("MIX").map(|(_, mix)| mix).collect();
        let channels = root
            .children_named("CHANNEL")
            .map(|(index, channel)| {
                let source = match channel.property("channelInputSource") {
                    Some(Var::Int(code)) => InputSource::from_code(*code),
                    _ => InputSource::Unknown,
                };
                ChannelState {
                    index,
                    source,
                    muted: match channel.property("channelOutputMute") {
                        Some(Var::Bool(muted)) => Some(*muted),
                        _ => None,
                    },
                    level: source.code().and_then(|code| mix_level(&mixes, code)),
                }
            })
            .collect();
        let fader_count = root
            .children_named("PHYSICALINTERFACE")
            .flat_map(|(_, panel)| panel.children_named("FADER"))
            .count();
        Self {
            firmware,
            channels,
            fader_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BoardState, ChannelState, InputSource, MIX_BUSES};
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

    fn mix(value: &str) -> Node {
        node(
            "MIX",
            vec![("mixLevelWithAnchor", Var::String(value.to_owned()))],
            vec![],
        )
    }

    /// The `MIX` group of one source: its first bus carries `value`, the
    /// others a level no test expects.
    fn mix_group(value: &str) -> Vec<Node> {
        std::iter::once(mix(value))
            .chain((1..MIX_BUSES).map(|_| mix("0.9|0.9")))
            .collect()
    }

    #[test]
    fn extracts_firmware_channels_and_faders() {
        let fader = |level| node("FADER", vec![("faderLevel", Var::Int(level))], vec![]);
        let mut children = vec![
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
            channel(1, false),
            channel(-1, false),
        ];
        children.extend(mix_group("0.2|0.354331"));
        children.extend(mix_group("0|1"));
        let state = BoardState::from_tree(&node("Rodecaster", vec![], children));
        assert_eq!(state.firmware.as_deref(), Some("1.7.6"));
        assert_eq!(state.fader_count, 2);
        assert_eq!(
            state.channels,
            [
                // N1: the second value of the first bus, × 127, rounded;
                // `faderLevel` (N6) is not read.
                ChannelState {
                    index: 2,
                    source: InputSource::Mic1,
                    muted: Some(true),
                    level: Some(45),
                },
                ChannelState {
                    index: 3,
                    source: InputSource::Code(1),
                    muted: Some(false),
                    level: Some(127),
                },
                // N3: an empty strip has no level.
                ChannelState {
                    index: 4,
                    source: InputSource::Empty,
                    muted: Some(false),
                    level: None,
                },
            ]
        );
    }

    /// N1: the `MIX` nodes are counted, not addressed by index (I-013).
    #[test]
    fn a_level_is_found_among_other_nodes() {
        let mut children = vec![channel(1, false)];
        children.extend(mix_group("0|0"));
        let mut second = mix_group("0|0.5");
        let tail = second.split_off(2);
        children.extend(second);
        children.push(node("STREAMERXSTREAMMIX", vec![], vec![]));
        children.extend(tail);
        let state = BoardState::from_tree(&node("Rodecaster", vec![], children));
        assert_eq!(state.channels[0].level, Some(64));
    }

    /// N2: a live change of the first bus is read.
    #[test]
    fn a_moved_fader_changes_the_level() {
        let mut children = vec![channel(0, false)];
        children.extend(mix_group("0|0"));
        let mut root = node("Rodecaster", vec![], children);
        crate::stream::apply_property(
            &mut root,
            &[1],
            "mixLevelWithAnchor",
            Var::String("0|0.299213".to_owned()),
        )
        .unwrap();
        assert_eq!(BoardState::from_tree(&root).channels[0].level, Some(38));
    }

    /// N4: an unreadable level is unknown for its strip only.
    #[test]
    fn unreadable_values_keep_their_place() {
        for (value, readable) in [
            (Var::String("0.5".to_owned()), false),
            (Var::String("0|x".to_owned()), false),
            (Var::String("0|1.5".to_owned()), false),
            (Var::String("0|-0.1".to_owned()), false),
            (Var::String("0|NaN".to_owned()), false),
            (Var::Double(0.5), false),
            (Var::String("0|0.5".to_owned()), true),
        ] {
            let mut children = vec![channel(0, false), channel(1, false)];
            children.push(node(
                "MIX",
                vec![("mixLevelWithAnchor", value.clone())],
                vec![],
            ));
            children.extend((1..MIX_BUSES).map(|_| mix("0|0")));
            children.extend(mix_group("0|1"));
            let state = BoardState::from_tree(&node("Rodecaster", vec![], children));
            let expected = readable.then_some(64);
            assert_eq!(state.channels[0].level, expected, "{value:?}");
            assert_eq!(state.channels[1].level, Some(127), "{value:?}");
        }
        // A source past the last group, or an unknown source.
        let mut children = vec![
            channel(1, false),
            node(
                "CHANNEL",
                vec![("channelInputSource", Var::Bool(true))],
                vec![],
            ),
        ];
        children.extend(mix_group("0|1"));
        let state = BoardState::from_tree(&node("Rodecaster", vec![], children));
        assert_eq!(state.channels[0].level, None);
        assert_eq!(state.channels[1].source, InputSource::Unknown);
        assert_eq!(state.channels[1].level, None);
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
