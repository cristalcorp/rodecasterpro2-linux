//! Input reports: handshake acknowledgement, state dump, change notifications.
//!
//! The board speaks JUCE `ValueTreeSynchroniser`: every message starts with a
//! change type (`1` property changed, `2` full sync, …). Report 4 carries two
//! shapes, interleaved (facts published by the AccessCaster project, verified
//! on a real board):
//!
//! - **dump chunk**: 255 raw bytes of one long message. The first chunk opens
//!   with the message length (u32, little-endian), then `2` (full sync) and the
//!   whole tree.
//! - **notification**: the length of one complete message (u32), the message,
//!   then zero padding.

use crate::tree::{DecodeError, Node, Reader, Var, decode_tree};
use crate::{PROPERTY_IN_REPORT_ID, PROPERTY_REPORT_DATA_LEN};

/// Report ID of the handshake acknowledgement (`A` followed by the device name).
pub const ACK_REPORT_ID: u8 = 2;

/// JUCE `ValueTreeSynchroniser` change types.
const CHANGE_PROPERTY: u8 = 1;
const CHANGE_FULL_SYNC: u8 = 2;

/// Largest state dump accepted, well above the ~150 KB of a real board.
const MAX_DUMP_LEN: usize = 4 << 20;

/// A message from the board.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    /// A property changed on the node at `path` (child indices from the root).
    PropertyChanged {
        /// Child index at each level, from the root.
        path: Vec<usize>,
        /// Property name.
        name: String,
        /// New value.
        value: Var,
    },
    /// The whole tree.
    FullSync(Node),
    /// Another change type (child added, removed, moved…), kept undecoded.
    Other {
        /// The change type byte.
        kind: u8,
    },
}

/// Decodes one complete message.
///
/// # Errors
///
/// Returns [`DecodeError`] if the message is malformed or has trailing bytes.
pub fn decode_change(message: &[u8]) -> Result<Change, DecodeError> {
    let (&kind, rest) = message.split_first().ok_or(DecodeError::Truncated(0))?;
    match kind {
        CHANGE_PROPERTY => {
            let mut reader = Reader::new(rest);
            let depth = reader.count()?;
            let mut path = Vec::with_capacity(depth.min(reader.remaining()));
            for _ in 0..depth {
                path.push(reader.count()?);
            }
            let name = reader.string()?;
            let value = reader.var()?;
            match reader.remaining() {
                0 => Ok(Change::PropertyChanged { path, name, value }),
                extra => Err(DecodeError::Trailing(extra)),
            }
        }
        CHANGE_FULL_SYNC => Ok(Change::FullSync(decode_tree(rest)?)),
        kind => Ok(Change::Other { kind }),
    }
}

/// What an input report holds.
#[derive(Debug, Clone, PartialEq)]
pub enum Incoming<'a> {
    /// Handshake acknowledgement (report 2).
    Ack,
    /// A complete notification.
    Change(Change),
    /// 255 raw bytes of the state dump.
    DumpChunk(&'a [u8]),
    /// Anything else (other report IDs, empty reports).
    Unknown,
}

/// Classifies one input report as read from `hidraw` (report ID first).
///
/// A report-4 payload counts as a notification only if it is exactly
/// `length, message, zeros` and the message decodes completely; anything else
/// is a dump chunk.
#[must_use]
pub fn classify(report: &[u8]) -> Incoming<'_> {
    let Some((&id, data)) = report.split_first() else {
        return Incoming::Unknown;
    };
    match id {
        ACK_REPORT_ID => Incoming::Ack,
        PROPERTY_IN_REPORT_ID => {
            notification(data).map_or(Incoming::DumpChunk(data), Incoming::Change)
        }
        _ => Incoming::Unknown,
    }
}

fn notification(data: &[u8]) -> Option<Change> {
    let len_bytes: [u8; 4] = data.get(..4)?.try_into().ok()?;
    let len = usize::try_from(u32::from_le_bytes(len_bytes)).ok()?;
    let message = data.get(4..4usize.checked_add(len)?)?;
    let padding = data.get(4 + len..)?;
    if len == 0 || padding.iter().any(|byte| *byte != 0) {
        return None;
    }
    // Only fully decoded property changes count: any other shape (including
    // the zero-padded last chunk of a dump) is treated as dump data.
    match decode_change(message).ok()? {
        change @ Change::PropertyChanged { .. } => Some(change),
        Change::FullSync(_) | Change::Other { .. } => None,
    }
}

/// Why a dump could not be assembled.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DumpError {
    /// The first chunk is too short to hold the length.
    #[error("first dump chunk too short")]
    NoLength,
    /// The announced length is implausible.
    #[error("announced dump length {0} is implausible")]
    BadLength(usize),
    /// The dump is not a full sync message.
    #[error("dump is not a full sync message (type {0})")]
    NotFullSync(u8),
    /// The tree inside is malformed.
    #[error("malformed dump: {0}")]
    Decode(#[from] DecodeError),
}

/// Collects dump chunks until the announced length is reached.
#[derive(Debug, Default)]
pub struct DumpAssembler {
    expected: Option<usize>,
    data: Vec<u8>,
}

impl DumpAssembler {
    /// Bytes still missing, if the dump has started.
    #[must_use]
    pub fn missing(&self) -> Option<usize> {
        self.expected
            .map(|expected| expected.saturating_sub(self.data.len()))
    }

    /// Adds one chunk (a report-4 payload, without the report ID). Returns the
    /// decoded tree once the dump is complete; extra bytes in the last chunk
    /// are padding.
    ///
    /// # Errors
    ///
    /// Returns [`DumpError`] if the length is implausible or the complete dump
    /// is not a well-formed full sync. The assembler is then reset.
    pub fn push(&mut self, chunk: &[u8]) -> Result<Option<Node>, DumpError> {
        let expected = if let Some(expected) = self.expected {
            self.data.extend_from_slice(chunk);
            expected
        } else {
            let len_bytes: [u8; 4] = chunk
                .get(..4)
                .and_then(|bytes| bytes.try_into().ok())
                .ok_or(DumpError::NoLength)?;
            let expected = usize::try_from(u32::from_le_bytes(len_bytes)).unwrap_or(usize::MAX);
            if !(2..=MAX_DUMP_LEN).contains(&expected) {
                return Err(DumpError::BadLength(expected));
            }
            self.expected = Some(expected);
            self.data = Vec::with_capacity(expected + PROPERTY_REPORT_DATA_LEN);
            self.data
                .extend_from_slice(chunk.get(4..).unwrap_or_default());
            expected
        };
        if self.data.len() < expected {
            return Ok(None);
        }
        let data = std::mem::take(&mut self.data);
        self.expected = None;
        let message = data.get(..expected).unwrap_or_default();
        match decode_change(message)? {
            Change::FullSync(root) => Ok(Some(root)),
            Change::PropertyChanged { .. } => Err(DumpError::NotFullSync(CHANGE_PROPERTY)),
            Change::Other { kind } => Err(DumpError::NotFullSync(kind)),
        }
    }
}

/// Why a change could not be applied to a tree.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("no node at path {0:?}: the local copy of the board state is out of date")]
pub struct PathNotFound(pub Vec<usize>);

/// Applies a property change to a tree (sets or adds the property).
///
/// # Errors
///
/// Returns [`PathNotFound`] if the path does not exist in `root`: the copy is
/// out of date and must be refreshed with a new dump.
pub fn apply_property(
    root: &mut Node,
    path: &[usize],
    name: &str,
    value: Var,
) -> Result<(), PathNotFound> {
    let mut node = root;
    for index in path {
        node = node
            .children
            .get_mut(*index)
            .ok_or_else(|| PathNotFound(path.to_vec()))?;
    }
    match node.properties.iter_mut().find(|(key, _)| key == name) {
        Some((_, current)) => *current = value,
        None => node.properties.push((name.to_owned(), value)),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        Change, DumpAssembler, DumpError, Incoming, apply_property, classify, decode_change,
    };
    use crate::tree::{Node, Var, encode_tree, write_int, write_var};

    fn tree() -> Node {
        Node {
            name: "Rodecaster".to_owned(),
            properties: vec![],
            children: (0..40)
                .map(|index| Node {
                    name: "CHANNEL".to_owned(),
                    properties: vec![
                        ("channelInputSource".to_owned(), Var::Int(index)),
                        ("channelOutputMute".to_owned(), Var::Bool(false)),
                    ],
                    children: vec![],
                })
                .collect(),
        }
    }

    /// Splits a full-sync message into report-4 chunks, as the board does.
    fn dump_reports(root: &Node) -> Vec<Vec<u8>> {
        let mut message = vec![2];
        message.extend(encode_tree(root));
        let mut stream = u32::try_from(message.len()).unwrap().to_le_bytes().to_vec();
        stream.extend(message);
        stream
            .chunks(255)
            .map(|chunk| {
                let mut report = vec![4];
                report.extend_from_slice(chunk);
                report.resize(256, 0);
                report
            })
            .collect()
    }

    fn property_message(path: &[i64], name: &str, value: &Var) -> Vec<u8> {
        let mut message = vec![1];
        write_int(&mut message, i64::try_from(path.len()).unwrap());
        for index in path {
            write_int(&mut message, *index);
        }
        message.extend_from_slice(name.as_bytes());
        message.push(0);
        write_var(&mut message, value);
        message
    }

    fn notification_report(message: &[u8]) -> Vec<u8> {
        let mut report = vec![4];
        report.extend(u32::try_from(message.len()).unwrap().to_le_bytes());
        report.extend_from_slice(message);
        report.resize(256, 0);
        report
    }

    #[test]
    fn assembles_a_dump_split_over_reports() {
        let root = tree();
        let reports = dump_reports(&root);
        assert!(reports.len() > 2);
        let mut assembler = DumpAssembler::default();
        let mut decoded = None;
        for report in &reports {
            let Incoming::DumpChunk(chunk) = classify(report) else {
                panic!("expected a dump chunk");
            };
            decoded = assembler.push(chunk).unwrap();
        }
        assert_eq!(decoded, Some(root));
        assert_eq!(assembler.missing(), None);
    }

    #[test]
    fn decodes_the_notification_shape_seen_on_the_board() {
        // rodey's example: 01 | path [0x1c] | "noiseGateOn" | true.
        let message = property_message(&[0x1C], "noiseGateOn", &Var::Bool(true));
        assert_eq!(message[..5], [0x01, 0x01, 0x01, 0x01, 0x1C]);
        let report = notification_report(&message);
        assert_eq!(
            classify(&report),
            Incoming::Change(Change::PropertyChanged {
                path: vec![0x1C],
                name: "noiseGateOn".to_owned(),
                value: Var::Bool(true)
            })
        );
    }

    #[test]
    fn notifications_interleaved_with_a_dump_are_kept_apart() {
        let root = tree();
        let mut reports = dump_reports(&root);
        let push = notification_report(&property_message(
            &[3],
            "channelOutputMute",
            &Var::Bool(true),
        ));
        reports.insert(1, push);
        let mut assembler = DumpAssembler::default();
        let (mut changes, mut decoded) = (0, None);
        for report in &reports {
            match classify(report) {
                Incoming::DumpChunk(chunk) => decoded = assembler.push(chunk).unwrap(),
                Incoming::Change(_) => changes += 1,
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(changes, 1);
        assert_eq!(decoded, Some(root));
    }

    #[test]
    fn a_padded_last_dump_chunk_is_not_a_notification() {
        // Looks like "length 42, message, zeros" but is not a property change.
        let mut report = vec![4, 42, 0, 0, 0, 0x07];
        report.extend([0x41; 41]);
        report.resize(256, 0);
        assert!(matches!(classify(&report), Incoming::DumpChunk(_)));
    }

    #[test]
    fn classifies_the_ack_and_ignores_other_reports() {
        assert_eq!(classify(&[2, b'A', b'R']), Incoming::Ack);
        assert_eq!(classify(&[9, 1, 2]), Incoming::Unknown);
        assert_eq!(classify(&[]), Incoming::Unknown);
    }

    #[test]
    fn rejects_implausible_dump_lengths() {
        let mut assembler = DumpAssembler::default();
        assert_eq!(
            assembler.push(&[0xFF, 0xFF, 0xFF, 0x7F, 2]),
            Err(DumpError::BadLength(0x7FFF_FFFF))
        );
        assert_eq!(assembler.push(&[1, 2]), Err(DumpError::NoLength));
    }

    #[test]
    fn applies_property_changes_by_path() {
        let mut root = tree();
        let Ok(Change::PropertyChanged { path, name, value }) = decode_change(&property_message(
            &[5],
            "channelOutputMute",
            &Var::Bool(true),
        )) else {
            panic!("expected a property change");
        };
        apply_property(&mut root, &path, &name, value).unwrap();
        assert_eq!(
            root.children[5].property("channelOutputMute"),
            Some(&Var::Bool(true))
        );
        assert!(apply_property(&mut root, &[99], "x", Var::Int(1)).is_err());
    }
}
