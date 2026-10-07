//! Rendering. Reads the state, never changes it.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Cell, Clear, List, ListItem, ListState, Paragraph, Row, Table, TableState,
};
use rcp2_audio::{Channel, PersistState};
use rcp2_proto::InputSource;

use super::app::{App, BoardShown, Focus, Level};
use crate::daemon::{ChannelDto, StateDto};
use crate::{channel_of, mute_label, output_label};

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
    let board = BoardPanel::new(app);
    // The console never takes the room of the body, the message or the keys,
    // and is left out when even its borders and one line do not fit.
    let room = frame.area().height.saturating_sub(1 + MIN_BODY + 1 + 1);
    let console_height = if room < board.min_height() {
        0
    } else {
        board.height().min(room)
    };
    let [header, body, console, message, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(MIN_BODY),
        Constraint::Length(console_height),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let [outputs, apps] =
        Layout::horizontal([Constraint::Length(30), Constraint::Min(40)]).areas(body);

    frame.render_widget(header_line(app, mode), header);
    render_outputs(frame, app, &board, outputs);
    render_apps(frame, app, apps);
    if console_height > 0 {
        render_console(frame, &board, console);
    }
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

fn render_outputs(frame: &mut Frame<'_>, app: &App, board: &BoardPanel<'_>, area: Rect) {
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
            if let Some(fader) = board.fader_of(channel) {
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

/// Smallest height of the Outputs/Applications body: every named output
/// and the two borders (checked against `Channel::ALL` by a test).
const MIN_BODY: u16 = 8;

/// The board's source feeding a named output, when known.
const fn channel_source(channel: Channel) -> Option<InputSource> {
    match channel {
        Channel::Chat => Some(InputSource::Chat),
        Channel::Usb1 => Some(InputSource::Usb1),
        Channel::Game => Some(InputSource::Game),
        Channel::Music => Some(InputSource::Music),
        Channel::A | Channel::B => None,
    }
}

/// One channel strip as shown, with its fader (strip `n` sits on fader
/// `n + 1`, verified on hardware; strips past the last fader have none).
struct Strip<'a> {
    fader: Option<usize>,
    channel: &'a ChannelDto,
    level: Option<i32>,
}

/// What the Console panel shows, computed once per frame.
enum BoardPanel<'a> {
    State {
        /// Strips worth showing (empty ones hidden, unreadable ones kept).
        strips: Vec<Strip<'a>>,
        /// Possibly no longer current: the board is read again, or the
        /// service did not answer the last poll.
        refreshing: bool,
    },
    Message(String),
}

impl<'a> BoardPanel<'a> {
    fn new(app: &'a App) -> Self {
        match &app.board {
            BoardShown::State { state, unanswered } => {
                let strips = strips(state);
                if strips.is_empty() {
                    Self::Message("No channel is assigned on the board.".to_owned())
                } else {
                    Self::State {
                        strips,
                        refreshing: state.refreshing || *unanswered,
                    }
                }
            }
            BoardShown::Message(text) => Self::Message(text.clone()),
        }
    }

    /// Fewest rows worth drawing: borders, and the header with one strip,
    /// or the one line of a message.
    const fn min_height(&self) -> u16 {
        match self {
            Self::State { .. } => 4,
            Self::Message(_) => 3,
        }
    }

    /// Rows wanted: borders, header and one per strip; one line otherwise.
    fn height(&self) -> u16 {
        match self {
            Self::State { strips, .. } => {
                u16::try_from(strips.len()).map_or(u16::MAX, |rows| rows.saturating_add(3))
            }
            Self::Message(_) => 3,
        }
    }

    /// The fader carrying a named output's channel, matched by source code.
    fn fader_of(&self, channel: Channel) -> Option<usize> {
        let code = channel_source(channel)?.code()?;
        match self {
            Self::State { strips, .. } => strips.iter().find_map(|strip| {
                (strip.channel.code == Some(code))
                    .then_some(strip.fader)
                    .flatten()
            }),
            Self::Message(_) => None,
        }
    }
}

fn strips(state: &StateDto) -> Vec<Strip<'_>> {
    let empty = InputSource::Empty.code();
    state
        .channels
        .iter()
        .enumerate()
        .filter(|(_, channel)| channel.code != empty)
        .map(|(position, channel)| Strip {
            fader: (position < state.faders.len()).then_some(position + 1),
            channel,
            level: state.faders.get(position).copied().flatten(),
        })
        .collect()
}

fn render_console(frame: &mut Frame<'_>, board: &BoardPanel<'_>, area: Rect) {
    let (strips, refreshing) = match board {
        BoardPanel::State { strips, refreshing } => (strips, *refreshing),
        BoardPanel::Message(text) => {
            frame.render_widget(
                Paragraph::new(text.as_str())
                    .style(Style::new().fg(Color::DarkGray))
                    .block(panel(" Console ", false)),
                area,
            );
            return;
        }
    };
    let title = if refreshing {
        " Console (refreshing…) "
    } else {
        " Console "
    };
    let rows = strips.iter().map(|strip| {
        let style = match strip.channel.muted {
            Some(true) => Style::new().fg(Color::Red),
            Some(false) => Style::new().fg(Color::Green),
            None => Style::new(),
        };
        let level = match (strip.fader, strip.level) {
            (Some(_), Some(level)) => fader_bar(level),
            (Some(_), None) => "?".to_owned(),
            (None, _) => String::new(),
        };
        Row::new(vec![
            Cell::from(
                strip
                    .fader
                    .map_or_else(|| "-".to_owned(), |fader| format!("F{fader}")),
            ),
            Cell::from(strip.channel.source.clone()),
            Cell::from(mute_label(strip.channel.muted)).style(style),
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
    .block(panel(title, false));
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
    use super::{MIN_BODY, Mode, keys_text, render};
    use crate::daemon::CLIENT_TIMEOUT;
    use crate::tui::REFRESH_EVERY;
    use crate::tui::app::FAILURE_GRACE;
    use crate::tui::app::Focus;
    use crate::tui::app::{App, BoardIssue};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use rcp2_audio::{Channel, Graph, PersistState};
    use std::time::{Duration, Instant};

    const DUMP: &str = include_str!("../../../rcp2-audio/tests/fixtures/pw-dump.json");

    fn screen(app: &App) -> String {
        screen_of_height(app, 24)
    }

    fn screen_of_height(app: &App, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(100, height)).unwrap();
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

    /// Records an answer one second after the previous one.
    fn answer(app: &mut App, view: crate::tui::app::BoardView, at: &mut Instant) {
        *at += Duration::from_secs(1);
        app.set_board(view, *at);
    }

    fn board_state(known: bool) -> crate::daemon::StateDto {
        use crate::daemon::{ChannelDto, StateDto};
        let strip = |index, source: &str, code, muted| ChannelDto {
            index,
            source: source.to_owned(),
            code,
            muted: Some(muted),
        };
        StateDto {
            version: crate::daemon::PROTOCOL_VERSION,
            connected: true,
            state_known: known,
            refreshing: false,
            firmware: Some("1.7.6".to_owned()),
            channels: if known {
                vec![
                    strip(0x1A, "Mic 1", Some(0), true),
                    strip(0x1B, "USB 1", Some(7), false),
                    strip(0x1C, "Chat", Some(8), false),
                    strip(0x1D, "Music", Some(13), false),
                    strip(0x1E, "Game", Some(12), false),
                    strip(0x1F, "(empty)", Some(-1), false),
                    strip(0x20, "?", None, false),
                    strip(0x103, "source 9", Some(9), false),
                ]
            } else {
                vec![]
            },
            faders: if known {
                vec![
                    Some(45),
                    Some(26),
                    Some(28),
                    Some(22),
                    Some(127),
                    Some(0),
                    None,
                ]
            } else {
                vec![]
            },
            notifications: 3,
        }
    }

    #[test]
    fn shows_the_board_and_the_fader_of_each_output() {
        let mut app = App::new(Graph::from_pw_dump(DUMP).unwrap());
        app.set_board(Ok(board_state(true)), Instant::now());
        let screen = screen(&app);
        assert!(screen.contains("3 RØDE Game   ♪1 ★ F5"), "{screen}");
        assert!(screen.contains("F1"), "{screen}");
        assert!(screen.contains("Mic 1"), "{screen}");
        assert!(screen.contains("muted"), "{screen}");
        assert!(screen.contains("████████████ 127"), "{screen}");
        // Empty strips are hidden; an unreadable source is shown, with an
        // unreadable level; a strip past the last fader has none.
        assert!(!screen.contains("(empty)"), "{screen}");
        assert!(screen.contains("F7     ?"), "{screen}");
        assert!(screen.contains("source 9"), "{screen}");
    }

    #[test]
    fn marks_the_previous_state_while_the_board_is_read_again() {
        let mut app = App::new(Graph::from_pw_dump(DUMP).unwrap());
        let mut refreshing = board_state(true);
        refreshing.refreshing = true;
        answer(&mut app, Ok(refreshing), &mut Instant::now());
        let screen = screen(&app);
        assert!(screen.contains("Console (refreshing…)"), "{screen}");
        assert!(screen.contains("F5"), "{screen}");
    }

    #[test]
    fn explains_why_the_board_state_is_missing() {
        let mut app = App::new(Graph::from_pw_dump(DUMP).unwrap());
        let at = &mut Instant::now();
        answer(&mut app, Err(BoardIssue::NotRunning), at);
        assert!(screen(&app).contains("rcp2ctl hid setup"));
        answer(
            &mut app,
            Err(BoardIssue::Other("socket timed out".to_owned())),
            at,
        );
        let screen = screen(&app);
        assert!(
            screen.contains("Board service: socket timed out"),
            "{screen}"
        );
        assert!(!screen.contains("hid setup"), "{screen}");
        answer(&mut app, Ok(board_state(false)), at);
        assert!(screen_of_height(&app, 24).contains(crate::BOARD_BEING_READ));
    }

    #[test]
    fn a_short_terminal_keeps_the_message_and_keys_lines() {
        let mut app = App::new(Graph::from_pw_dump(DUMP).unwrap());
        app.set_board(Ok(board_state(true)), Instant::now());
        app.message = Some((crate::tui::app::Level::Info, "hello there".to_owned()));
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).unwrap();
        let mode = Mode {
            outputs_on: true,
            persist: Some(PersistState::Off),
        };
        terminal.draw(|frame| render(frame, &app, mode)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(text.contains("hello there"));
        assert!(text.contains("q quit"));
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

    #[test]
    fn says_when_the_board_is_unplugged() {
        let mut app = App::new(Graph::from_pw_dump(DUMP).unwrap());
        let at = &mut Instant::now();
        answer(&mut app, Ok(board_state(true)), at);
        let mut unplugged = board_state(false);
        unplugged.connected = false;
        answer(&mut app, Ok(unplugged), at);
        let screen = screen(&app);
        assert!(screen.contains(crate::BOARD_NOT_CONNECTED), "{screen}");
        assert!(!screen.contains('{'), "{screen}");
        assert!(!screen.contains("F5"), "{screen}");
    }

    #[test]
    fn a_failed_poll_keeps_the_last_state_for_a_moment() {
        let mut app = App::new(Graph::from_pw_dump(DUMP).unwrap());
        let start = Instant::now();
        app.set_board(Ok(board_state(true)), start);
        // When real polls that time out end: time counts, not polls.
        let poll = REFRESH_EVERY + CLIENT_TIMEOUT;
        app.set_board(Err(BoardIssue::Timeout), start + poll);
        let screen_now = screen(&app);
        assert!(screen_now.contains("Console (refreshing…)"), "{screen_now}");
        assert!(screen_now.contains("F5"), "{screen_now}");
        app.set_board(Err(BoardIssue::Timeout), start + FAILURE_GRACE);
        let screen_now = screen(&app);
        assert!(
            screen_now.contains("Board service: no answer in time"),
            "{screen_now}"
        );
        // A later answer shows the state again, as current.
        app.set_board(Ok(board_state(true)), start + FAILURE_GRACE + poll);
        let screen_now = screen(&app);
        assert!(!screen_now.contains("refreshing"), "{screen_now}");
        assert!(screen_now.contains("F5"), "{screen_now}");
    }

    #[test]
    fn an_error_other_than_a_timeout_is_said_at_once() {
        let mut app = App::new(Graph::from_pw_dump(DUMP).unwrap());
        let at = &mut Instant::now();
        answer(&mut app, Ok(board_state(true)), at);
        answer(
            &mut app,
            Err(BoardIssue::Other("invalid answer".to_owned())),
            at,
        );
        let screen = screen(&app);
        assert!(screen.contains("Board service: invalid answer"), "{screen}");
        assert!(!screen.contains("F5"), "{screen}");
    }

    #[test]
    fn another_protocol_version_is_said_at_once() {
        let mut app = App::new(Graph::from_pw_dump(DUMP).unwrap());
        let at = &mut Instant::now();
        answer(&mut app, Ok(board_state(true)), at);
        let reason = "older than this rcp2ctl".to_owned();
        answer(&mut app, Err(BoardIssue::Other(reason)), at);
        let screen = screen(&app);
        assert!(
            screen.contains("Board service: older than this rcp2ctl"),
            "{screen}"
        );
        assert!(!screen.contains("F5"), "{screen}");
    }

    #[test]
    fn a_board_without_channels_says_so() {
        let mut app = App::new(Graph::from_pw_dump(DUMP).unwrap());
        let mut bare = board_state(true);
        bare.channels.clear();
        answer(&mut app, Ok(bare), &mut Instant::now());
        let screen = screen(&app);
        assert!(screen.contains("No channel is assigned"), "{screen}");
    }

    #[test]
    fn the_body_always_fits_every_output() {
        let rows = u16::try_from(Channel::ALL.len()).unwrap() + 2;
        assert_eq!(MIN_BODY, rows);
        let mut app = App::new(Graph::from_pw_dump(DUMP).unwrap());
        app.set_board(Ok(board_state(true)), Instant::now());
        // The console wants 10 rows here; the body keeps its 8.
        let screen = screen_of_height(&app, 20);
        assert!(screen.contains("6 RØDE B"), "{screen}");
        assert!(screen.contains("Console"), "{screen}");
    }

    #[test]
    fn a_tiny_terminal_leaves_the_console_out() {
        let mut app = App::new(Graph::from_pw_dump(DUMP).unwrap());
        app.set_board(Ok(board_state(true)), Instant::now());
        app.message = Some((crate::tui::app::Level::Info, "hello there".to_owned()));
        let screen = screen_of_height(&app, 12);
        assert!(screen.contains("hello there"), "{screen}");
        assert!(screen.contains("q quit"), "{screen}");
        assert!(screen.contains("6 RØDE B"), "{screen}");
        assert!(!screen.contains("Console"), "{screen}");
    }

    #[test]
    fn a_console_without_room_for_one_strip_is_left_out() {
        let mut app = App::new(Graph::from_pw_dump(DUMP).unwrap());
        app.set_board(Ok(board_state(true)), Instant::now());
        // 3 rows free: borders and header, no strip.
        assert!(!screen_of_height(&app, 14).contains("Console"));
        assert!(screen_of_height(&app, 15).contains("Mic 1"));
    }
}
