//! Behaviour of the graph model against a trimmed, anonymised `pw-dump` capture.

use rcp2_audio::{Channel, DetectError, Graph, pipewire_config};

const DUMP: &str = include_str!("fixtures/pw-dump.json");
const EXPECTED_CONFIG: &str = include_str!("fixtures/virtual-sinks.conf");

const STEREO: &str = "alsa_output.usb-R__DE_RODECaster_Pro_II_TESTSERIAL-00.pro-output-0";
const MULTI: &str = "alsa_output.usb-R__DE_RODECaster_Pro_II_TESTSERIAL-00.pro-output-1";

#[expect(
    clippy::unwrap_used,
    reason = "test helper: an invalid fixture must abort the test"
)]
fn graph() -> Graph {
    Graph::from_pw_dump(DUMP).unwrap()
}

/// Removes the objects whose `id` is in `ids` from the fixture.
#[expect(
    clippy::unwrap_used,
    reason = "test helper: an invalid fixture must abort the test"
)]
fn graph_without(ids: &[u64]) -> Graph {
    let mut objects: Vec<serde_json::Value> = serde_json::from_str(DUMP).unwrap();
    objects.retain(|object| !ids.contains(&object["id"].as_u64().unwrap()));
    Graph::from_pw_dump(&serde_json::to_string(&objects).unwrap()).unwrap()
}

#[test]
fn detects_the_board_native_sinks() {
    let rode = graph().rode().unwrap();
    assert_eq!(rode.stereo_sink, STEREO);
    assert_eq!(rode.multi_sink, MULTI);
}

#[test]
fn reports_a_missing_board() {
    assert_eq!(graph_without(&[72]).rode(), Err(DetectError::NotFound));
}

#[test]
fn reports_a_board_outside_the_pro_audio_profile() {
    assert_eq!(
        graph_without(&[89]).rode(),
        Err(DetectError::MissingSink {
            profile: "pro-output-1"
        })
    );
}

#[test]
fn rejects_an_unexpected_channel_layout() {
    let dump = DUMP.replace("\"audio.channels\": 10", "\"audio.channels\": 8");
    assert_eq!(
        Graph::from_pw_dump(&dump).unwrap().rode(),
        Err(DetectError::UnexpectedLayout {
            profile: "pro-output-1",
            expected: 10,
            found: 8
        })
    );
}

#[test]
fn finds_installed_virtual_sinks_only() {
    let graph = graph();
    assert_eq!(graph.virtual_sink(Channel::Game).unwrap().serial, Some(154));
    assert!(graph.virtual_sink(Channel::Music).is_none());
}

#[test]
fn lists_app_streams_without_our_own_loopbacks() {
    let graph = graph();
    let labels: Vec<_> = graph
        .app_streams()
        .iter()
        .map(|s| s.app_label().to_owned())
        .collect();
    // Skipped: the node without a name, our rcp2.game.output loopback, and the
    // filter chain's internal stream (it has a node.link-group).
    assert_eq!(labels, ["Firefox", "spotify", "PipeWire ALSA [qbz]"]);
}

#[test]
fn reports_where_each_app_plays_once_per_target() {
    let graph = graph();
    let streams = graph.app_streams();
    let targets = |label: &str| -> Vec<String> {
        streams
            .iter()
            .find(|s| s.app_label() == label)
            .unwrap()
            .targets
            .iter()
            .map(|n| n.name.clone())
            .collect()
    };
    // Two links (one per channel) to the same sink count once.
    assert_eq!(targets("Firefox"), [STEREO]);
    assert_eq!(targets("spotify"), ["rcp2.game"]);
}

#[test]
fn accepts_serials_given_as_strings() {
    let graph = graph();
    let firefox = graph.nodes().iter().find(|n| n.name == "Firefox").unwrap();
    assert_eq!(firefox.serial, Some(415));
}

#[test]
fn selects_streams_by_id_app_or_binary_ignoring_case() {
    let graph = graph();
    let streams = graph.app_streams();
    let selected = |selector: &str| -> Vec<u32> {
        streams
            .iter()
            .filter(|s| s.matches(selector))
            .map(|s| s.node.id)
            .collect()
    };
    assert_eq!(selected("300"), [300]);
    assert_eq!(selected("FIREFOX"), [300]);
    assert_eq!(selected("spotify"), [301]);
    // Binary known only through the stream's client (PipeWire ALSA plugin).
    assert_eq!(selected("qbz"), [303]);
    assert!(
        selected("effect_output.eq").is_empty(),
        "internal streams are not routable"
    );
    assert!(selected("fire").is_empty(), "no substring matching");
}

#[test]
fn generates_the_reference_config() {
    let rode = graph().rode().unwrap();
    assert_eq!(pipewire_config(&rode).unwrap(), EXPECTED_CONFIG);
}

#[test]
fn rejects_input_that_is_not_a_pw_dump() {
    assert!(Graph::from_pw_dump("{\"not\": \"an array\"}").is_err());
}

#[test]
fn reads_the_default_sink_from_metadata() {
    assert_eq!(graph().default_sink(), Some("rcp2.game"));
    assert_eq!(graph_without(&[30]).default_sink(), None);
}
