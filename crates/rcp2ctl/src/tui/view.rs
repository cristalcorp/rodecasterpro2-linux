//! Rendering. Reads the state, never changes it.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
};
use rcp2_audio::{Channel, PersistState};

use super::app::{App, Focus, Level};
use crate::daemon::{ChannelDto, StateDto};
use crate::{channel_of, output_label};

/// Golden yellow accent.
const ACCENT: Color = Color::Rgb(255, 191, 0);

/// What the header shows about how the outputs are kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mode {
    pub(crate) outputs_on: bool,
    /// `None` if the state could not be read (the error is shown separately).
    pub(crate) persist: Option<PersistState>,
}

pub(crate) fn render(frame: &mut Frame<'_>, app: &App, mode: Mode) {
    let [header, body, console, message, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(8),
        Constraint::Length(console_height(app)),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let [outputs, apps] =
        Layout::horizontal([Constraint::Length(30), Constraint::Min(40)]).areas(body);

    frame.render_widget(header_line(app, mode), header);
    render_outputs(frame, app, outputs);
    render_apps(frame, app, apps);
    render_console(frame, app, console);
    frame.render_widget(message_line(app), message);
    frame.render_widget(keys_line(app), keys);
    if app.show_help {
        render_help(frame);
    }
}

fn header_line(app: &App, mode: Mode) -> Paragraph<'static> {
    let board = if app.graph.rode().is_ok() {
        Span::styled(
            "RØDECaster Pro II",
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        )
    } else {
        Span::styled("RØDECaster Pro II not found", Style::new().fg(Color::Red))
    };
    let outputs = match (mode.persist, mode.outputs_on) {
        (None, _) => "outputs: ? (cannot read the config state)",
        (Some(PersistState::On), _) => "outputs: on, kept by config file",
        (Some(_), true) => "outputs: on, created at launch",
        (Some(_), false) => "outputs: off",
    };
    let default = default_channel(app).map_or_else(
        || {
            app.graph
                .default_sink()
                .and_then(|name| app.graph.nodes().iter().find(|node| node.name == name))
                .and_then(|node| node.description.clone())
                .unwrap_or_else(|| "?".to_owned())
        },
        Channel::description,
    );
    Paragraph::new(Line::from(vec![
        board,
        Span::raw("  ·  "),
        Span::raw(outputs),
        Span::raw("  ·  default: "),
        Span::styled(default, Style::new().fg(ACCENT)),
    ]))
}

/// The named output the system's default output plays on.
fn default_channel(app: &App) -> Option<Channel> {
    let default = app.graph.default_sink()?;
    let node = app.graph.nodes().iter().find(|node| node.name == default)?;
    channel_of(&app.graph, node)
}

fn panel(title: &str, focused: bool) -> Block<'_> {
    let style = if focused {
        Style::new().fg(ACCENT)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    Block::bordered().title(title).border_style(style)
}

fn render_outputs(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let streams = app.graph.app_streams();
    let default = default_channel(app);
    let items: Vec<ListItem<'_>> = Channel::ALL
        .into_iter()
        .zip(1..)
        .map(|(channel, number)| {
            let present = app.graph.virtual_sink(channel).is_some();
            let playing = streams
                .iter()
                .filter(|stream| {
                    stream
                        .targets
                        .iter()
                        .any(|node| channel_of(&app.graph, node) == Some(channel))
                })
                .count();
            let mut spans = vec![
                Span::styled(format!("{number} "), Style::new().fg(ACCENT)),
                Span::raw(format!("{:<11}", channel.description())),
            ];
            if !present {
                spans.push(Span::styled(" absent", Style::new().fg(Color::Red)));
            } else if playing > 0 {
                spans.push(Span::raw(format!(" ♪{playing}")));
            }
            if default == Some(channel) {
                spans.push(Span::styled(" ★", Style::new().fg(ACCENT)));
            }
            if let Some(fader) = fader_of(app, channel) {
                spans.push(Span::styled(
                    format!(" F{fader}"),
                    Style::new().fg(Color::DarkGray),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let focused = app.focus == Focus::Outputs;
    let list = List::new(items)
        .block(panel(" Outputs ", focused))
        .highlight_style(selection_style(focused));
    let mut state = ListState::default().with_selected(Some(app.output_selected));
    frame.render_stateful_widget(list, area, &mut state);
}

/// The board's name for the source feeding a named output, when known.
const fn source_label(channel: Channel) -> Option<&'static str> {
    match channel {
        Channel::Chat => Some("Chat"),
        Channel::Usb1 => Some("USB 1"),
        Channel::Game => Some("Game"),
        Channel::Music => Some("Music"),
        Channel::A | Channel::B => None,
    }
}

/// The board's channel strips worth showing, with their fader number
/// (strip `n` sits on fader `n + 1`, verified on hardware; strips beyond the
/// last fader have none).
fn strips(state: &StateDto) -> Vec<(Option<usize>, &ChannelDto)> {
    state
        .channels
        .iter()
        .enumerate()
        .filter(|(_, channel)| channel.source != "(empty)")
        .map(|(position, channel)| {
            (
                (position < state.faders.len()).then_some(position + 1),
                channel,
            )
        })
        .collect()
}

fn known_state(app: &App) -> Option<&StateDto> {
    match &app.board {
        Some(Ok(state)) if state.connected && state.state_known => Some(state),
        _ => None,
    }
}

/// The fader carrying a named output's channel on the board, if known.
fn fader_of(app: &App, channel: Channel) -> Option<usize> {
    let label = source_label(channel)?;
    strips(known_state(app)?)
        .into_iter()
        .find(|(_, strip)| strip.source == label)
        .and_then(|(fader, _)| fader)
}

fn console_height(app: &App) -> u16 {
    // Borders plus header row plus one row per strip; one line otherwise.
    known_state(app).map_or(3, |state| {
        u16::try_from(strips(state).len()).map_or(3, |rows| rows.saturating_add(3))
    })
}

fn render_console(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let block = panel(" Console ", false);
    let Some(state) = known_state(app) else {
        let text = match &app.board {
            Some(Ok(state)) if state.connected => "Reading the board's state…",
            Some(Ok(_)) => "Board service running; the board is not connected.",
            Some(Err(_)) | None => "Board service not running: `rcp2ctl hid setup` installs it.",
        };
        frame.render_widget(
            Paragraph::new(text)
                .style(Style::new().fg(Color::DarkGray))
                .block(block),
            area,
        );
        return;
    };
    let rows = strips(state).into_iter().map(|(fader, strip)| {
        let (output, style) = match strip.muted {
            Some(true) => ("muted", Style::new().fg(Color::Red)),
            Some(false) => ("on", Style::new().fg(Color::Green)),
            None => ("?", Style::new()),
        };
        let level = fader
            .and_then(|fader| state.faders.get(fader - 1))
            .map_or_else(String::new, |level| fader_bar(*level));
        Row::new(vec![
            Cell::from(fader.map_or_else(|| "-".to_owned(), |fader| format!("F{fader}"))),
            Cell::from(strip.source.clone()),
            Cell::from(output).style(style),
            Cell::from(level),
        ])
    });
    let header = Row::new(["FADER", "SOURCE", "OUTPUT", "LEVEL (last full read)"])
        .style(Style::new().add_modifier(Modifier::BOLD));
    let table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Length(12),
            Constraint::Length(7),
            Constraint::Min(20),
        ],
    )
    .header(header)
    .block(block);
    frame.render_widget(table, area);
}

/// A 0–127 fader level as a 12-cell bar and its value.
fn fader_bar(level: i32) -> String {
    let filled = usize::try_from(level.clamp(0, 127) * 12 / 127).unwrap_or(0);
    format!("{}{} {level}", "█".repeat(filled), "·".repeat(12 - filled))
}

fn render_apps(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let streams = app.graph.app_streams();
    let focused = app.focus == Focus::Apps;
    let block = panel(" Applications ", focused);
    if streams.is_empty() {
        let empty = Paragraph::new("No application is playing audio.").block(block);
        frame.render_widget(empty, area);
        return;
    }
    let rows = streams.iter().map(|stream| {
        let output = output_label(&app.graph, stream);
        Row::new(vec![
            Cell::from(stream.node.id.to_string()),
            Cell::from(
                stream
                    .node
                    .process_binary
                    .clone()
                    .unwrap_or_else(|| "-".to_owned()),
            ),
            Cell::from(stream.app_label().to_owned()),
            Cell::from(output),
        ])
    });
    let header = Row::new(["ID", "BINARY", "APPLICATION", "OUTPUT"])
        .style(Style::new().add_modifier(Modifier::BOLD));
    let table = Table::new(
        rows,
        [
            Constraint::Length(6),
            Constraint::Length(14),
            Constraint::Min(16),
            Constraint::Length(22),
        ],
    )
    .header(header)
    .block(block)
    .row_highlight_style(selection_style(focused));
    let mut state = TableState::default().with_selected(Some(app.stream_selected));
    frame.render_stateful_widget(table, area, &mut state);
}

fn selection_style(focused: bool) -> Style {
    if focused {
        Style::new().fg(Color::Black).bg(ACCENT)
    } else {
        Style::new().add_modifier(Modifier::REVERSED)
    }
}

fn message_line(app: &App) -> Paragraph<'_> {
    match &app.message {
        Some((Level::Error, text)) => {
            Paragraph::new(text.as_str()).style(Style::new().fg(Color::Red))
        }
        Some((Level::Info, text)) => Paragraph::new(text.as_str()),
        None => Paragraph::new(""),
    }
}

fn keys_line(app: &App) -> Paragraph<'static> {
    Paragraph::new(keys_text(app)).style(Style::new().fg(Color::DarkGray))
}

/// Fits 80 columns, so `? help` and `q quit` stay visible.
fn keys_text(app: &App) -> String {
    let context = match app.focus {
        Focus::Apps => "1-6 route",
        Focus::Outputs => "⏎ default",
    };
    format!("↑↓ move  {context}  Tab panel  o outputs  p persist  r restore  ? help  q quit")
}

const HELP: &[(&str, &str)] = &[
    ("↑ ↓ / j k", "select"),
    ("Tab ← →", "switch panel"),
    ("1 … 6", "send the selected application to that output"),
    ("Enter / d", "make the selected output the default"),
    ("o", "named outputs on / off"),
    ("p", "keep outputs after reboot (install the config file)"),
    ("r", "restore the original PipeWire configuration"),
    ("F5 / Ctrl+L", "refresh now (also automatic every second)"),
    ("q / Esc", "quit"),
];

fn render_help(frame: &mut Frame<'_>) {
    let area = centered(frame.area(), 70, 14);
    let mut lines: Vec<Line<'_>> = HELP
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!("{key:<12}"), Style::new().fg(ACCENT)),
                Span::raw(*what),
            ])
        })
        .collect();
    lines.push(Line::raw(""));
    lines.push(Line::raw(
        "★ default  ♪ apps playing  Fn fader on the board  any key closes",
    ));
    frame.render_widget(Clear, area);
    frame.render_widget(Paragraph::new(lines).block(panel(" Help ", true)), area);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    let [area] = Layout::horizontal([Constraint::Length(width)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    let [area] = Layout::vertical([Constraint::Length(height)])
        .flex(ratatui::layout::Flex::Center)
        .areas(area);
    area
}

#[cfg(test)]
mod tests {
    use super::{Mode, keys_text, render};
    use crate::tui::app::App;
    use crate::tui::app::Focus;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use rcp2_audio::{Graph, PersistState};

    const DUMP: &str = include_str!("../../../rcp2-audio/tests/fixtures/pw-dump.json");

    fn screen(app: &App) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        let mode = Mode {
            outputs_on: true,
            persist: Some(PersistState::Off),
        };
        terminal.draw(|frame| render(frame, app, mode)).unwrap();
        let buffer = terminal.backend().buffer();
        buffer
            .content()
            .chunks(usize::from(buffer.area.width))
            .map(|row| {
                row.iter()
                    .map(ratatui::buffer::Cell::symbol)
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn shows_outputs_apps_and_the_default() {
        let app = App::new(Graph::from_pw_dump(DUMP).unwrap());
        let screen = screen(&app);
        assert!(screen.contains("default: RØDE Game"), "{screen}");
        assert!(screen.contains("RØDECaster Pro II"), "{screen}");
        assert!(
            screen.contains("outputs: on, created at launch"),
            "{screen}"
        );
        // Game exists in the fixture, is the default, and spotify plays on it.
        assert!(screen.contains("3 RØDE Game   ♪1 ★"), "{screen}");
        assert!(screen.contains("4 RØDE Music  absent"), "{screen}");
        assert!(screen.contains("qbz"), "{screen}");
        assert!(screen.contains("PipeWire ALSA [qbz]"), "{screen}");
    }

    #[test]
    fn shows_the_board_and_the_fader_of_each_output() {
        use crate::daemon::{ChannelDto, StateDto};
        let mut app = App::new(Graph::from_pw_dump(DUMP).unwrap());
        let strip = |index, source: &str, muted| ChannelDto {
            index,
            source: source.to_owned(),
            muted: Some(muted),
        };
        app.board = Some(Ok(StateDto {
            version: 1,
            connected: true,
            state_known: true,
            firmware: Some("1.7.6".to_owned()),
            channels: vec![
                strip(0x1A, "Mic 1", true),
                strip(0x1B, "USB 1", false),
                strip(0x1C, "Chat", false),
                strip(0x1D, "Music", false),
                strip(0x1E, "Game", false),
                strip(0x1F, "(empty)", false),
                strip(0x103, "source 9", false),
            ],
            faders: vec![45, 26, 28, 22, 127, 0],
            notifications: 3,
        }));
        let screen = screen(&app);
        assert!(screen.contains("3 RØDE Game   ♪1 ★ F5"), "{screen}");
        assert!(screen.contains("F1"), "{screen}");
        assert!(screen.contains("Mic 1"), "{screen}");
        assert!(screen.contains("muted"), "{screen}");
        assert!(screen.contains("████████████ 127"), "{screen}");
        // Empty strips are hidden; a strip past the last fader has none.
        assert!(!screen.contains("(empty)"), "{screen}");
        assert!(screen.contains("source 9"), "{screen}");
    }

    #[test]
    fn says_how_to_start_the_service_when_it_is_not_running() {
        let mut app = App::new(Graph::from_pw_dump(DUMP).unwrap());
        app.board = Some(Err("not running".to_owned()));
        assert!(screen(&app).contains("rcp2ctl hid setup"));
    }

    #[test]
    fn help_overlays_the_screen() {
        let mut app = App::new(Graph::from_pw_dump(DUMP).unwrap());
        app.show_help = true;
        assert!(screen(&app).contains("restore the original PipeWire configuration"));
    }

    #[test]
    fn the_keys_line_fits_80_columns_in_both_panels() {
        for focus in [Focus::Apps, Focus::Outputs] {
            let mut app = App::new(Graph::from_pw_dump(DUMP).unwrap());
            app.focus = focus;
            let width = ratatui::text::Line::from(keys_text(&app)).width();
            assert!(width <= 80, "{width} columns");
        }
    }
}
