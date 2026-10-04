//! The board's state as a JUCE `ValueTree`, and its binary serialisation.
//!
//! Format (as written by JUCE `ValueTree::writeToStream`):
//!
//! ```text
//! node     = name\0  count(properties)  (name\0 var)*  count(children)  node*
//! var      = count(size)  marker  data            (size includes the marker)
//! count    = n  byte*n                              (n bytes, little-endian;
//!                                                    bit 7 of n = negative)
//! ```
//!
//! Markers: `1` int32, `2` true, `3` false, `4` float64, `5` string (UTF-8,
//! NUL-terminated), `6` int64, `8` binary. Verified exactly against full
//! dumps of a real board (firmware 1.6.8 and 1.7.6).

/// Deepest nesting accepted, so hostile input cannot exhaust the stack. Real
/// dumps are a few levels deep.
pub const MAX_DEPTH: usize = 32;

/// Largest count accepted (properties, children, sizes), far above any real
/// dump, so hostile input cannot request huge allocations.
const MAX_COUNT: u32 = 1 << 24;

/// A property value.
#[derive(Debug, Clone, PartialEq)]
pub enum Var {
    /// Marker `1`.
    Int(i32),
    /// Markers `2` (true) and `3` (false).
    Bool(bool),
    /// Marker `4`.
    Double(f64),
    /// Marker `5`.
    String(String),
    /// Marker `6`.
    Int64(i64),
    /// Marker `8`.
    Binary(Vec<u8>),
    /// Any other marker, kept verbatim so nothing is silently lost.
    Other {
        /// The marker byte.
        marker: u8,
        /// The bytes after it.
        data: Vec<u8>,
    },
}

/// A node of the tree.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Node {
    /// Node type, e.g. `CHANNEL`.
    pub name: String,
    /// Properties in serialisation order.
    pub properties: Vec<(String, Var)>,
    /// Children in serialisation order: their index is part of the address
    /// the board uses for changes.
    pub children: Vec<Node>,
}

impl Node {
    /// The value of property `name`, if present.
    #[must_use]
    pub fn property(&self, name: &str) -> Option<&Var> {
        self.properties
            .iter()
            .find_map(|(key, value)| (key == name).then_some(value))
    }

    /// Children of type `name`, with their index under this node.
    pub fn children_named<'a>(&'a self, name: &'a str) -> impl Iterator<Item = (usize, &'a Node)> {
        self.children
            .iter()
            .enumerate()
            .filter(move |(_, child)| child.name == name)
    }
}

/// Why bytes could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// The input ended in the middle of a value.
    #[error("input ends early at byte {0}")]
    Truncated(usize),
    /// A count uses more bytes than JUCE ever writes.
    #[error("invalid count at byte {0}")]
    BadCount(usize),
    /// A count is negative or implausibly large.
    #[error("count out of range at byte {0}")]
    CountOutOfRange(usize),
    /// A name or string is not valid UTF-8.
    #[error("invalid UTF-8 at byte {0}")]
    Utf8(usize),
    /// A value's declared size does not fit its type.
    #[error("value of marker {marker} has a wrong size at byte {at}")]
    BadValue {
        /// The marker.
        marker: u8,
        /// Where the value starts.
        at: usize,
    },
    /// Nesting deeper than [`MAX_DEPTH`].
    #[error("tree deeper than {MAX_DEPTH} levels")]
    TooDeep,
    /// Bytes remain after the root node.
    #[error("{0} unexpected trailing bytes")]
    Trailing(usize),
}

/// A cursor over the input, with bounds checks on every read.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    pub(crate) pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    pub(crate) fn remaining(&self) -> usize {
        self.bytes.len().saturating_sub(self.pos)
    }

    pub(crate) fn byte(&mut self) -> Result<u8, DecodeError> {
        let byte = *self
            .bytes
            .get(self.pos)
            .ok_or(DecodeError::Truncated(self.pos))?;
        self.pos += 1;
        Ok(byte)
    }

    pub(crate) fn take(&mut self, len: usize) -> Result<&'a [u8], DecodeError> {
        let end = self
            .pos
            .checked_add(len)
            .ok_or(DecodeError::Truncated(self.pos))?;
        let slice = self
            .bytes
            .get(self.pos..end)
            .ok_or(DecodeError::Truncated(self.pos))?;
        self.pos = end;
        Ok(slice)
    }

    /// A JUCE compressed int (at most 4 value bytes).
    pub(crate) fn int(&mut self) -> Result<i64, DecodeError> {
        let at = self.pos;
        let head = self.byte()?;
        let len = usize::from(head & 0x7F);
        if len > 4 {
            return Err(DecodeError::BadCount(at));
        }
        let mut value: i64 = 0;
        for (shift, byte) in self.take(len)?.iter().enumerate() {
            value |= i64::from(*byte) << (8 * shift);
        }
        Ok(if head & 0x80 == 0 { value } else { -value })
    }

    /// A count: a compressed int that must be non-negative and plausible.
    pub(crate) fn count(&mut self) -> Result<usize, DecodeError> {
        let at = self.pos;
        let value = self.int()?;
        u32::try_from(value)
            .ok()
            .filter(|value| *value <= MAX_COUNT)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or(DecodeError::CountOutOfRange(at))
    }

    /// A NUL-terminated UTF-8 string.
    pub(crate) fn string(&mut self) -> Result<String, DecodeError> {
        let at = self.pos;
        let rest = self
            .bytes
            .get(self.pos..)
            .ok_or(DecodeError::Truncated(at))?;
        let len = rest
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(DecodeError::Truncated(at))?;
        let text = self.take(len)?;
        self.pos += 1;
        String::from_utf8(text.to_vec()).map_err(|_| DecodeError::Utf8(at))
    }

    pub(crate) fn var(&mut self) -> Result<Var, DecodeError> {
        let size = self.count()?;
        let at = self.pos;
        let bytes = self.take(size)?;
        let (&marker, data) = bytes
            .split_first()
            .ok_or(DecodeError::BadValue { marker: 0, at })?;
        let bad = || DecodeError::BadValue { marker, at };
        Ok(match marker {
            1 => Var::Int(i32::from_le_bytes(data.try_into().map_err(|_| bad())?)),
            2 if data.is_empty() => Var::Bool(true),
            3 if data.is_empty() => Var::Bool(false),
            4 => Var::Double(f64::from_le_bytes(data.try_into().map_err(|_| bad())?)),
            5 => {
                let text = data.strip_suffix(&[0]).ok_or_else(bad)?;
                Var::String(String::from_utf8(text.to_vec()).map_err(|_| DecodeError::Utf8(at))?)
            }
            6 => Var::Int64(i64::from_le_bytes(data.try_into().map_err(|_| bad())?)),
            8 => Var::Binary(data.to_vec()),
            2 | 3 => return Err(bad()),
            _ => Var::Other {
                marker,
                data: data.to_vec(),
            },
        })
    }

    fn node(&mut self, depth: usize) -> Result<Node, DecodeError> {
        if depth > MAX_DEPTH {
            return Err(DecodeError::TooDeep);
        }
        let name = self.string()?;
        let property_count = self.count()?;
        // Capacity bounded by what the remaining input can actually hold.
        let mut properties = Vec::with_capacity(property_count.min(self.remaining() / 3));
        for _ in 0..property_count {
            let key = self.string()?;
            properties.push((key, self.var()?));
        }
        let child_count = self.count()?;
        let mut children = Vec::with_capacity(child_count.min(self.remaining() / 3));
        for _ in 0..child_count {
            children.push(self.node(depth + 1)?);
        }
        Ok(Node {
            name,
            properties,
            children,
        })
    }
}

/// Decodes one serialised node (and its subtree) that spans all of `bytes`.
///
/// # Errors
///
/// Returns [`DecodeError`] on malformed input, nesting deeper than
/// [`MAX_DEPTH`], or bytes left over after the node.
pub fn decode_tree(bytes: &[u8]) -> Result<Node, DecodeError> {
    let mut reader = Reader::new(bytes);
    let node = reader.node(0)?;
    match reader.remaining() {
        0 => Ok(node),
        extra => Err(DecodeError::Trailing(extra)),
    }
}

/// Appends a JUCE compressed int.
pub(crate) fn write_int(out: &mut Vec<u8>, value: i64) {
    let magnitude = value.unsigned_abs();
    let bytes = magnitude.to_le_bytes();
    let len = bytes
        .iter()
        .rposition(|byte| *byte != 0)
        .map_or(0, |last| last + 1);
    // JUCE writes at most 4 value bytes; larger values are not representable.
    let len = len.min(4);
    let mut head = u8::try_from(len).unwrap_or(4);
    if value < 0 {
        head |= 0x80;
    }
    out.push(head);
    out.extend(bytes.iter().take(len));
}

fn write_count(out: &mut Vec<u8>, count: usize) {
    write_int(out, i64::try_from(count).unwrap_or(i64::MAX));
}

fn write_string(out: &mut Vec<u8>, text: &str) {
    out.extend_from_slice(text.as_bytes());
    out.push(0);
}

pub(crate) fn write_var(out: &mut Vec<u8>, value: &Var) {
    let mut body = Vec::new();
    match value {
        Var::Int(value) => {
            body.push(1);
            body.extend_from_slice(&value.to_le_bytes());
        }
        Var::Bool(true) => body.push(2),
        Var::Bool(false) => body.push(3),
        Var::Double(value) => {
            body.push(4);
            body.extend_from_slice(&value.to_le_bytes());
        }
        Var::String(text) => {
            body.push(5);
            write_string(&mut body, text);
        }
        Var::Int64(value) => {
            body.push(6);
            body.extend_from_slice(&value.to_le_bytes());
        }
        Var::Binary(data) => {
            body.push(8);
            body.extend_from_slice(data);
        }
        Var::Other { marker, data } => {
            body.push(*marker);
            body.extend_from_slice(data);
        }
    }
    write_count(out, body.len());
    out.extend(body);
}

/// Serialises a node and its subtree, as JUCE does. Used to build test
/// fixtures and, later, change messages.
#[must_use]
pub fn encode_tree(node: &Node) -> Vec<u8> {
    let mut out = Vec::new();
    write_node(&mut out, node);
    out
}

fn write_node(out: &mut Vec<u8>, node: &Node) {
    write_string(out, &node.name);
    write_count(out, node.properties.len());
    for (key, value) in &node.properties {
        write_string(out, key);
        write_var(out, value);
    }
    write_count(out, node.children.len());
    for child in &node.children {
        write_node(out, child);
    }
}

#[cfg(test)]
mod tests {
    use super::{DecodeError, MAX_DEPTH, Node, Var, decode_tree, encode_tree, write_int};

    fn sample() -> Node {
        Node {
            name: "Rodecaster".to_owned(),
            properties: vec![],
            children: vec![
                Node {
                    name: "CHANNEL".to_owned(),
                    properties: vec![
                        ("channelInputSource".to_owned(), Var::Int(0)),
                        ("channelOutputMute".to_owned(), Var::Bool(true)),
                        ("compressorGain".to_owned(), Var::Double(0.25)),
                        ("label".to_owned(), Var::String("RØDE".to_owned())),
                        ("big".to_owned(), Var::Int64(-5_000_000_000)),
                        ("blob".to_owned(), Var::Binary(vec![0, 1, 2])),
                    ],
                    children: vec![],
                },
                Node {
                    name: "FADER".to_owned(),
                    properties: vec![("faderLevel".to_owned(), Var::Int(45))],
                    children: vec![],
                },
            ],
        }
    }

    #[test]
    fn round_trips_every_value_type() {
        let tree = sample();
        assert_eq!(decode_tree(&encode_tree(&tree)).unwrap(), tree);
    }

    #[test]
    fn decodes_the_bytes_seen_on_the_board() {
        // The start of a real dump: POT { potMin = 0, potMax = 127, potLevel = 58 }.
        let bytes = b"POT\0\x01\x03potMin\0\x01\x05\x01\x00\x00\x00\x00\
potMax\0\x01\x05\x01\x7f\x00\x00\x00potLevel\0\x01\x05\x01\x3a\x00\x00\x00\x00";
        let pot = decode_tree(bytes).unwrap();
        assert_eq!(pot.name, "POT");
        assert_eq!(pot.property("potMax"), Some(&Var::Int(127)));
        assert_eq!(pot.property("potLevel"), Some(&Var::Int(58)));
        assert!(pot.children.is_empty());
    }

    #[test]
    fn booleans_follow_juce_markers() {
        // 2 = true, 3 = false (rodey has them swapped, see I-009).
        let node = |marker: u8| {
            let mut bytes = b"N\0\x01\x01m\0\x01\x01".to_vec();
            bytes.push(marker);
            bytes.push(0);
            decode_tree(&bytes).unwrap()
        };
        assert_eq!(node(2).property("m"), Some(&Var::Bool(true)));
        assert_eq!(node(3).property("m"), Some(&Var::Bool(false)));
    }

    #[test]
    fn compressed_ints_round_trip() {
        for value in [0, 1, 127, 128, 366, 702, 65_535, 1 << 24, -1, -300] {
            let mut bytes = Vec::new();
            write_int(&mut bytes, value);
            let mut reader = super::Reader::new(&bytes);
            assert_eq!(reader.int().unwrap(), value);
            assert_eq!(reader.remaining(), 0);
        }
    }

    #[test]
    fn rejects_malformed_input() {
        assert!(matches!(decode_tree(b""), Err(DecodeError::Truncated(_))));
        assert!(matches!(
            decode_tree(b"X\0\x05"),
            Err(DecodeError::BadCount(_))
        ));
        // Negative property count.
        assert!(matches!(
            decode_tree(b"X\0\x81\x01"),
            Err(DecodeError::CountOutOfRange(_))
        ));
        // An int32 declared with 3 bytes of data.
        assert!(matches!(
            decode_tree(b"X\0\x01\x01m\0\x01\x04\x01\x00\x00\x00\x00"),
            Err(DecodeError::BadValue { marker: 1, .. })
        ));
        // A string without its terminator.
        assert!(matches!(
            decode_tree(b"X\0\x01\x01m\0\x01\x02\x05a\x00"),
            Err(DecodeError::BadValue { marker: 5, .. })
        ));
        let mut trailing = encode_tree(&sample());
        trailing.push(0);
        assert_eq!(decode_tree(&trailing), Err(DecodeError::Trailing(1)));
    }

    #[test]
    fn refuses_nesting_beyond_the_limit() {
        let mut deep = Node::default();
        for _ in 0..=MAX_DEPTH {
            deep = Node {
                name: "N".to_owned(),
                properties: vec![],
                children: vec![deep],
            };
        }
        assert_eq!(decode_tree(&encode_tree(&deep)), Err(DecodeError::TooDeep));
    }
}
