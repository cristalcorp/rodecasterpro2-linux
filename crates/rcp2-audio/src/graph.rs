//! Read-only model of the PipeWire graph, parsed from `pw-dump` JSON output.

use std::collections::{HashMap, HashSet};

use serde::Deserialize;
use serde_json::{Map, Value};

use crate::channel::{Channel, NativeSink};

/// USB vendor ID of RØDE Microphones, as PipeWire reports it.
const RODE_VENDOR_ID: &str = "0x19f7";
/// `device.product.name` of the board.
const PRODUCT_NAME: &str = "RODECaster Pro II";
/// Prefix of every node this project creates.
const OWN_NODE_PREFIX: &str = "rcp2.";

/// Error while parsing `pw-dump` output.
#[derive(Debug, thiserror::Error)]
#[error("invalid pw-dump output: {0}")]
pub struct ParseError(#[from] serde_json::Error);

/// Error while locating the board in the graph.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DetectError {
    /// No RØDECaster Pro II device in the graph.
    #[error("no RØDECaster Pro II found (is it plugged in and powered on?)")]
    NotFound,
    /// More than one board: which one to drive is ambiguous.
    #[error("{0} RØDECaster Pro II devices found; only one is supported")]
    Multiple(usize),
    /// The board is present but a native sink is missing.
    #[error(
        "the RØDECaster Pro II has no `{profile}` sink: select the \"Pro Audio\" profile for it \
         (e.g. in pavucontrol, Configuration tab)"
    )]
    MissingSink {
        /// Expected `device.profile.name`.
        profile: &'static str,
    },
    /// A native sink does not have the expected channel count.
    #[error("sink `{profile}` has {found} channels, expected {expected}")]
    UnexpectedLayout {
        /// `device.profile.name` of the sink.
        profile: &'static str,
        /// Expected channel count.
        expected: u64,
        /// Channel count found, 0 if unknown.
        found: u64,
    },
}

/// A PipeWire node, with only the properties this project uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Object ID (runtime only).
    pub id: u32,
    /// `object.serial`, used as a routing target.
    pub serial: Option<u64>,
    /// `node.name`.
    pub name: String,
    /// `node.description`.
    pub description: Option<String>,
    /// `media.class`, e.g. `Audio/Sink` or `Stream/Output/Audio`.
    pub media_class: Option<String>,
    /// `device.id`: owning device object, if any.
    pub device_id: Option<u64>,
    /// `device.profile.name`, e.g. `pro-output-1`.
    pub profile_name: Option<String>,
    /// `audio.channels`.
    pub channels: Option<u64>,
    /// `application.name`, from the node or else from its client.
    pub app_name: Option<String>,
    /// `application.process.binary`, from the node or else from its client
    /// (streams going through PipeWire's ALSA plugin only carry it there).
    pub process_binary: Option<String>,
    /// `media.name`, e.g. the page or track being played.
    pub media_name: Option<String>,
    /// Whether the node belongs to a `node.link-group`: one half of a loopback,
    /// filter chain or echo canceller, never an application.
    pub internal: bool,
}

/// A link between two nodes (data flows from `output_node` to `input_node`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Link {
    output_node: u32,
    input_node: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Device {
    id: u32,
    vendor_id: Option<String>,
    product_name: Option<String>,
}

/// Node names of the board's native sinks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rode {
    /// `node.name` of the 2-channel sink (`pro-output-0`).
    pub stereo_sink: String,
    /// `node.name` of the 10-channel sink (`pro-output-1`).
    pub multi_sink: String,
}

impl Rode {
    /// `node.name` of the given native sink.
    #[must_use]
    pub fn sink(&self, sink: NativeSink) -> &str {
        match sink {
            NativeSink::Stereo => &self.stereo_sink,
            NativeSink::Multi => &self.multi_sink,
        }
    }
}

/// An application playback stream and the nodes it currently plays to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppStream<'a> {
    /// The stream node.
    pub node: &'a Node,
    /// Nodes the stream is linked to (usually one sink).
    pub targets: Vec<&'a Node>,
}

impl AppStream<'_> {
    /// Name to show for the application. The process binary comes first because
    /// it is what [`AppStream::matches`] accepts and what users type; the
    /// application name can be generic (e.g. `PipeWire ALSA [qbz]`).
    #[must_use]
    pub fn app_label(&self) -> &str {
        self.node
            .process_binary
            .as_deref()
            .or(self.node.app_name.as_deref())
            .unwrap_or(&self.node.name)
    }

    /// Whether `selector` designates this stream: its object ID, or its
    /// application name, process binary or node name (case-insensitive).
    #[must_use]
    pub fn matches(&self, selector: &str) -> bool {
        if let Ok(id) = selector.parse::<u32>() {
            return self.node.id == id;
        }
        [
            self.node.app_name.as_deref(),
            self.node.process_binary.as_deref(),
            Some(self.node.name.as_str()),
        ]
        .into_iter()
        .flatten()
        .any(|name| name.eq_ignore_ascii_case(selector))
    }
}

/// Snapshot of the PipeWire graph.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Graph {
    nodes: Vec<Node>,
    devices: Vec<Device>,
    links: Vec<Link>,
}

#[derive(Deserialize)]
struct RawObject {
    id: u32,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    info: Option<RawInfo>,
}

#[derive(Deserialize)]
struct RawInfo {
    #[serde(default)]
    props: Option<Map<String, Value>>,
    #[serde(rename = "output-node-id")]
    output_node_id: Option<u32>,
    #[serde(rename = "input-node-id")]
    input_node_id: Option<u32>,
}

fn prop_str(props: &Map<String, Value>, key: &str) -> Option<String> {
    props.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// PipeWire sometimes serialises integers as strings: accept both.
fn prop_u64(props: &Map<String, Value>, key: &str) -> Option<u64> {
    match props.get(key)? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

impl Graph {
    /// Parses the JSON printed by `pw-dump`.
    ///
    /// Objects this project does not use are ignored, as are nodes without a
    /// `node.name`.
    ///
    /// # Errors
    ///
    /// Returns [`ParseError`] if the input is not a JSON array of PipeWire objects.
    pub fn from_pw_dump(json: &str) -> Result<Self, ParseError> {
        let objects: Vec<RawObject> = serde_json::from_str(json)?;
        let mut graph = Self::default();
        let mut clients: HashMap<u64, Map<String, Value>> = HashMap::new();
        let mut node_clients: Vec<Option<u64>> = Vec::new();
        for object in objects {
            let Some(info) = object.info else { continue };
            match object.kind.as_str() {
                "PipeWire:Interface:Node" => {
                    let Some(props) = info.props else { continue };
                    let Some(name) = prop_str(&props, "node.name") else {
                        continue;
                    };
                    node_clients.push(prop_u64(&props, "client.id"));
                    graph.nodes.push(Node {
                        id: object.id,
                        serial: prop_u64(&props, "object.serial"),
                        name,
                        description: prop_str(&props, "node.description"),
                        media_class: prop_str(&props, "media.class"),
                        device_id: prop_u64(&props, "device.id"),
                        profile_name: prop_str(&props, "device.profile.name"),
                        channels: prop_u64(&props, "audio.channels"),
                        app_name: prop_str(&props, "application.name"),
                        process_binary: prop_str(&props, "application.process.binary"),
                        media_name: prop_str(&props, "media.name"),
                        internal: props.contains_key("node.link-group"),
                    });
                }
                "PipeWire:Interface:Device" => {
                    let Some(props) = info.props else { continue };
                    graph.devices.push(Device {
                        id: object.id,
                        vendor_id: prop_str(&props, "device.vendor.id"),
                        product_name: prop_str(&props, "device.product.name"),
                    });
                }
                "PipeWire:Interface:Client" => {
                    if let Some(props) = info.props {
                        clients.insert(u64::from(object.id), props);
                    }
                }
                "PipeWire:Interface:Link" => {
                    if let (Some(output_node), Some(input_node)) =
                        (info.output_node_id, info.input_node_id)
                    {
                        graph.links.push(Link {
                            output_node,
                            input_node,
                        });
                    }
                }
                _ => {}
            }
        }
        // Clients may appear before or after their nodes: resolve once all are read.
        for (node, client_id) in graph.nodes.iter_mut().zip(node_clients) {
            let Some(client) = client_id.and_then(|id| clients.get(&id)) else {
                continue;
            };
            if node.app_name.is_none() {
                node.app_name = prop_str(client, "application.name");
            }
            if node.process_binary.is_none() {
                node.process_binary = prop_str(client, "application.process.binary");
            }
        }
        Ok(graph)
    }

    /// All nodes in the graph.
    #[must_use]
    pub fn nodes(&self) -> &[Node] {
        &self.nodes
    }

    /// Locates the board and its two native sinks.
    ///
    /// # Errors
    ///
    /// Returns [`DetectError`] if the board is absent, ambiguous, not in the
    /// `pro-audio` profile, or exposes an unexpected channel layout.
    pub fn rode(&self) -> Result<Rode, DetectError> {
        let boards: Vec<&Device> = self
            .devices
            .iter()
            .filter(|device| {
                device
                    .vendor_id
                    .as_deref()
                    .is_some_and(|id| id.eq_ignore_ascii_case(RODE_VENDOR_ID))
                    && device.product_name.as_deref() == Some(PRODUCT_NAME)
            })
            .collect();
        let board = match boards.as_slice() {
            [] => return Err(DetectError::NotFound),
            [board] => *board,
            _ => return Err(DetectError::Multiple(boards.len())),
        };
        Ok(Rode {
            stereo_sink: self.native_sink(board.id, NativeSink::Stereo)?,
            multi_sink: self.native_sink(board.id, NativeSink::Multi)?,
        })
    }

    fn native_sink(&self, device_id: u32, sink: NativeSink) -> Result<String, DetectError> {
        let profile = sink.profile_name();
        let node = self
            .nodes
            .iter()
            .find(|node| {
                node.device_id == Some(u64::from(device_id))
                    && node.media_class.as_deref() == Some("Audio/Sink")
                    && node.profile_name.as_deref() == Some(profile)
            })
            .ok_or(DetectError::MissingSink { profile })?;
        let expected = sink.channel_count();
        let found = node.channels.unwrap_or(0);
        if found != expected {
            return Err(DetectError::UnexpectedLayout {
                profile,
                expected,
                found,
            });
        }
        Ok(node.name.clone())
    }

    /// The named virtual sink for `channel`, if it exists.
    #[must_use]
    pub fn virtual_sink(&self, channel: Channel) -> Option<&Node> {
        self.nodes.iter().find(|node| {
            node.name.strip_prefix(OWN_NODE_PREFIX) == Some(channel.id())
                && node.media_class.as_deref() == Some("Audio/Sink")
        })
    }

    /// Application playback streams: excludes the internal streams of
    /// loopbacks and filters (ours included), which must not be rerouted.
    #[must_use]
    pub fn app_streams(&self) -> Vec<AppStream<'_>> {
        self.nodes
            .iter()
            .filter(|node| {
                node.media_class.as_deref() == Some("Stream/Output/Audio")
                    && !node.internal
                    && !node.name.starts_with(OWN_NODE_PREFIX)
            })
            .map(|node| AppStream {
                node,
                targets: self.targets_of(node.id),
            })
            .collect()
    }

    /// Nodes `node_id` sends audio to, deduplicated, in graph order.
    fn targets_of(&self, node_id: u32) -> Vec<&Node> {
        let inputs: HashSet<u32> = self
            .links
            .iter()
            .filter(|link| link.output_node == node_id)
            .map(|link| link.input_node)
            .collect();
        self.nodes
            .iter()
            .filter(|candidate| inputs.contains(&candidate.id))
            .collect()
    }
}
