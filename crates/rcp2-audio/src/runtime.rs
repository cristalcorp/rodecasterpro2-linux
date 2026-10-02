//! Named outputs created at runtime inside PipeWire's PulseAudio server
//! (`pipewire-pulse`): no file is written, they appear at once and live until
//! they are removed or PipeWire restarts.

use crate::channel::Channel;
use crate::config::{UnsafeNodeName, check_node_name};
use crate::graph::{Graph, Rode};
use crate::pw::{PwError, run};

/// PulseAudio module creating a sink mapped onto some channels of another one.
const REMAP_MODULE: &str = "module-remap-sink";

/// `pactl` arguments creating the named sink for `channel`: a stereo sink copied,
/// without remixing, onto the channel's pair of the board's native sink.
///
/// # Errors
///
/// Returns [`UnsafeNodeName`] if the native sink name contains characters that
/// could not be passed safely as a module argument.
pub fn remap_sink_args(channel: Channel, rode: &Rode) -> Result<Vec<String>, UnsafeNodeName> {
    let master = check_node_name(rode.sink(channel.native_sink()))?;
    let [left, right] = channel.positions();
    Ok(vec![
        "load-module".to_owned(),
        REMAP_MODULE.to_owned(),
        format!("sink_name={}", channel.sink_name()),
        format!("master={master}"),
        "channels=2".to_owned(),
        "channel_map=front-left,front-right".to_owned(),
        format!(
            "master_channel_map={},{}",
            left.to_ascii_lowercase(),
            right.to_ascii_lowercase()
        ),
        "remix=no".to_owned(),
        // Module arguments split on spaces unless quoted; the properties list
        // needs its own level of quoting for a value with spaces.
        format!(
            "sink_properties='device.description=\"{}\"'",
            channel.description()
        ),
    ])
}

/// Modules created by this project in `pactl list short modules` output
/// (`<id>\t<name>\t<arguments>`), with the channel each one serves.
fn runtime_modules(list: &str) -> Vec<(u32, Channel)> {
    list.lines()
        .filter_map(|line| {
            let mut fields = line.splitn(3, '\t');
            let id = fields.next()?.trim().parse().ok()?;
            if fields.next()? != REMAP_MODULE {
                return None;
            }
            let channel = fields
                .next()?
                .split_whitespace()
                .find_map(|arg| arg.strip_prefix("sink_name="))
                .and_then(Channel::from_sink_name)?;
            Some((id, channel))
        })
        .collect()
}

/// Creates at runtime the named outputs missing from `graph`, and returns the
/// channels created.
///
/// # Errors
///
/// Returns [`PwError`] if `pactl` cannot run or refuses a module. Outputs
/// created before the failure stay in place.
pub fn create_runtime_outputs(graph: &Graph, rode: &Rode) -> Result<Vec<Channel>, PwError> {
    let mut created = Vec::new();
    for channel in Channel::ALL {
        if graph.virtual_sink(channel).is_none() {
            run("pactl", &remap_sink_args(channel, rode)?)?;
            created.push(channel);
        }
    }
    Ok(created)
}

/// Removes the named outputs this project created at runtime, and returns
/// their channels. Outputs declared by the PipeWire config file are not
/// affected: they only go away when PipeWire restarts without the file.
///
/// # Errors
///
/// Returns [`PwError`] if `pactl` cannot run or fails to unload a module.
pub fn remove_runtime_outputs() -> Result<Vec<Channel>, PwError> {
    let list = run("pactl", &["list", "short", "modules"])?;
    let mut removed = Vec::new();
    for (id, channel) in runtime_modules(&String::from_utf8_lossy(&list)) {
        run("pactl", &["unload-module", &id.to_string()])?;
        removed.push(channel);
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::{remap_sink_args, runtime_modules};
    use crate::channel::Channel;
    use crate::graph::Rode;

    fn rode() -> Rode {
        Rode {
            stereo_sink: "alsa_output.usb-R__DE_RODECaster_Pro_II_TESTSERIAL-00.pro-output-0"
                .to_owned(),
            multi_sink: "alsa_output.usb-R__DE_RODECaster_Pro_II_TESTSERIAL-00.pro-output-1"
                .to_owned(),
        }
    }

    #[test]
    fn maps_a_channel_onto_its_native_pair() {
        let args = remap_sink_args(Channel::Game, &rode()).unwrap();
        assert!(args.contains(&"sink_name=rcp2.game".to_owned()));
        assert!(args.contains(&format!("master={}", rode().multi_sink)));
        assert!(args.contains(&"master_channel_map=aux2,aux3".to_owned()));
        assert!(args.contains(&"remix=no".to_owned()));
        assert!(args.contains(&"sink_properties='device.description=\"RØDE Game\"'".to_owned()));
    }

    #[test]
    fn chat_targets_the_stereo_sink() {
        let args = remap_sink_args(Channel::Chat, &rode()).unwrap();
        assert!(args.contains(&format!("master={}", rode().stereo_sink)));
        assert!(args.contains(&"master_channel_map=aux0,aux1".to_owned()));
    }

    #[test]
    fn refuses_an_unsafe_master_name() {
        let mut bad = rode();
        bad.multi_sink = "x remix=yes".to_owned();
        assert!(remap_sink_args(Channel::Game, &bad).is_err());
    }

    #[test]
    fn finds_only_our_remap_modules() {
        let list = "\
536870912\tlibpipewire-module-protocol-pulse\t
536870916\tmodule-remap-sink\tsink_name=rcp2.game master=x channels=2 sink_properties='device.description=\"RØDE Game\"'
536870917\tmodule-remap-sink\tsink_name=someone.else master=y
536870918\tmodule-null-sink\tsink_name=rcp2.music
536870919\tmodule-remap-sink\tsink_name=rcp2.b master=x
";
        assert_eq!(
            runtime_modules(list),
            [(536_870_916, Channel::Game), (536_870_919, Channel::B)]
        );
    }
}
