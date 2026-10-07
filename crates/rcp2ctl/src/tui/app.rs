//! TUI state and key handling: pure, no I/O, so it is tested without a terminal.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use rcp2_audio::{Channel, Graph};

use crate::daemon::StateDto;

/// Which panel receives the arrow keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Focus {
    Apps,
    Outputs,
}

/// Something the event loop must do in response to a key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    None,
    Quit,
    Refresh,
    /// Send the stream with this ID to a named output.
    Route {
        stream_id: u32,
        channel: Channel,
    },
    /// Make a named output the system's default output.
    SetDefault(Channel),
    ToggleOutputs,
    /// Install the PipeWire config file keeping the outputs after a reboot.
    EnablePersist,
    /// Put back the original PipeWire configuration.
    RestoreOriginal,
}

/// Severity of the message shown under the panels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Level {
    Info,
    Error,
}

pub(crate) struct App {
    pub(crate) graph: Graph,
    pub(crate) focus: Focus,
    pub(crate) stream_selected: usize,
    pub(crate) output_selected: usize,
    pub(crate) message: Option<(Level, String)>,
    pub(crate) show_help: bool,
    /// The board's own state from the board service, or why it is missing.
    pub(crate) board: Option<BoardView>,
    /// The last complete state, shown (as refreshing) while the service
    /// reads the board again, so the screen does not jump.
    pub(crate) last_known: Option<StateDto>,
    /// Failed polls in a row since the last answer from the service.
    failed_polls: u8,
}

/// Failed polls in a row (one a second) still shown as the last state:
/// a busy service is not worth a flicker, a lasting failure is worth saying.
const TOLERATED_FAILURES: u8 = 3;

/// Why the board state is unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BoardIssue {
    /// No board service answers.
    NotRunning,
    /// The service speaks another protocol version, with what to do.
    Version(String),
    /// Anything else, with the reason to show.
    Other(String),
}

/// What the board service said: its state, or why it could not be asked.
pub(crate) type BoardView = Result<StateDto, BoardIssue>;

impl App {
    pub(crate) fn new(graph: Graph) -> Self {
        Self {
            graph,
            focus: Focus::Apps,
            stream_selected: 0,
            output_selected: 0,
            message: None,
            show_help: false,
            board: None,
            last_known: None,
            failed_polls: 0,
        }
    }

    /// Records the board service's answer, keeping the last complete state
    /// for as long as the same board session lasts, and through a few
    /// failed polls.
    pub(crate) fn set_board(&mut self, view: BoardView) {
        if matches!(view, Err(BoardIssue::Other(_))) {
            self.failed_polls = self.failed_polls.saturating_add(1);
        } else {
            self.failed_polls = 0;
        }
        match &view {
            Ok(state) if state.connected && state.state_known => {
                self.last_known = Some(state.clone());
            }
            // Same session, being read again: the last state still holds.
            Ok(state)
                if state.connected
                    && self
                        .last_known
                        .as_ref()
                        .is_some_and(|last| last.session == state.session) => {}
            Err(BoardIssue::Other(_)) if self.failed_polls <= TOLERATED_FAILURES => {}
            _ => self.last_known = None,
        }
        self.board = Some(view);
    }

    /// Replaces the graph, keeping the selection on the same stream when it
    /// still exists.
    pub(crate) fn set_graph(&mut self, graph: Graph) {
        let selected_id = self.selected_stream_id();
        self.graph = graph;
        let streams = self.graph.app_streams();
        self.stream_selected = selected_id
            .and_then(|id| streams.iter().position(|stream| stream.node.id == id))
            .unwrap_or_else(|| self.stream_selected.min(streams.len().saturating_sub(1)));
    }

    pub(crate) fn selected_stream_id(&self) -> Option<u32> {
        self.graph
            .app_streams()
            .get(self.stream_selected)
            .map(|stream| stream.node.id)
    }

    pub(crate) fn selected_output(&self) -> Option<Channel> {
        Channel::ALL.get(self.output_selected).copied()
    }

    pub(crate) fn on_key(&mut self, key: KeyEvent) -> Action {
        if key.kind != KeyEventKind::Press {
            return Action::None;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Action::Quit;
        }
        if self.show_help {
            // Any other key closes the help.
            self.show_help = false;
            return Action::None;
        }
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('?') => {
                self.show_help = true;
                Action::None
            }
            KeyCode::F(5) => Action::Refresh,
            // For terminals that keep F5 for themselves; the usual redraw key.
            KeyCode::Char('l') if key.modifiers.contains(KeyModifiers::CONTROL) => Action::Refresh,
            KeyCode::Char('r') => Action::RestoreOriginal,
            KeyCode::Char('o') => Action::ToggleOutputs,
            KeyCode::Char('p') => Action::EnablePersist,
            KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
                self.focus = match self.focus {
                    Focus::Apps => Focus::Outputs,
                    Focus::Outputs => Focus::Apps,
                };
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_selection(false);
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_selection(true);
                Action::None
            }
            KeyCode::Char(digit @ '1'..='9') if self.focus == Focus::Apps => {
                self.route_to_digit(digit)
            }
            KeyCode::Enter | KeyCode::Char('d') if self.focus == Focus::Outputs => self
                .selected_output()
                .map_or(Action::None, Action::SetDefault),
            _ => Action::None,
        }
    }

    fn move_selection(&mut self, down: bool) {
        let (selected, len) = match self.focus {
            Focus::Apps => (&mut self.stream_selected, self.graph.app_streams().len()),
            Focus::Outputs => (&mut self.output_selected, Channel::ALL.len()),
        };
        if len == 0 {
            *selected = 0;
        } else if down {
            *selected = (*selected + 1).min(len - 1);
        } else {
            *selected = selected.saturating_sub(1);
        }
    }

    /// Digits route the selected application to the output with that number.
    fn route_to_digit(&mut self, digit: char) -> Action {
        let channel = digit
            .to_digit(10)
            .and_then(|n| usize::try_from(n).ok())
            .and_then(|n| n.checked_sub(1))
            .and_then(|index| Channel::ALL.get(index).copied());
        match (self.selected_stream_id(), channel) {
            (Some(stream_id), Some(channel)) => Action::Route { stream_id, channel },
            (None, _) => {
                self.message = Some((Level::Info, "No application is playing audio.".to_owned()));
                Action::None
            }
            (_, None) => Action::None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Action, App, Focus};
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use rcp2_audio::{Channel, Graph};

    const DUMP: &str = include_str!("../../../rcp2-audio/tests/fixtures/pw-dump.json");

    fn app() -> App {
        App::new(Graph::from_pw_dump(DUMP).unwrap())
    }

    fn press(app: &mut App, code: KeyCode) -> Action {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn digits_route_the_selected_app() {
        let mut app = app();
        // The fixture lists Firefox (300), spotify (301), qbz (303).
        press(&mut app, KeyCode::Down);
        assert_eq!(
            press(&mut app, KeyCode::Char('4')),
            Action::Route {
                stream_id: 301,
                channel: Channel::Music
            }
        );
        assert_eq!(press(&mut app, KeyCode::Char('7')), Action::None);
    }

    #[test]
    fn selection_stays_in_bounds() {
        let mut app = app();
        press(&mut app, KeyCode::Up);
        assert_eq!(app.stream_selected, 0);
        for _ in 0..10 {
            press(&mut app, KeyCode::Down);
        }
        assert_eq!(app.stream_selected, 2);
    }

    #[test]
    fn default_output_is_set_from_the_outputs_panel_only() {
        let mut app = app();
        assert_eq!(press(&mut app, KeyCode::Enter), Action::None);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.focus, Focus::Outputs);
        press(&mut app, KeyCode::Down);
        assert_eq!(
            press(&mut app, KeyCode::Enter),
            Action::SetDefault(Channel::Usb1)
        );
    }

    #[test]
    fn persistence_has_one_key_per_direction() {
        let mut app = app();
        assert_eq!(press(&mut app, KeyCode::Char('p')), Action::EnablePersist);
        assert_eq!(press(&mut app, KeyCode::Char('p')), Action::EnablePersist);
        assert_eq!(press(&mut app, KeyCode::Char('r')), Action::RestoreOriginal);
        assert_eq!(press(&mut app, KeyCode::F(5)), Action::Refresh);
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL)),
            Action::Refresh
        );
    }

    #[test]
    fn digits_do_nothing_in_the_outputs_panel() {
        let mut app = app();
        press(&mut app, KeyCode::Tab);
        assert_eq!(press(&mut app, KeyCode::Char('3')), Action::None);
    }

    #[test]
    fn ctrl_c_quits_even_from_the_help() {
        let mut app = app();
        press(&mut app, KeyCode::Char('?'));
        assert_eq!(
            app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Action::Quit
        );
    }

    #[test]
    fn help_swallows_the_next_key() {
        let mut app = app();
        press(&mut app, KeyCode::Char('?'));
        assert!(app.show_help);
        assert_eq!(press(&mut app, KeyCode::Char('q')), Action::None);
        assert!(!app.show_help);
        assert_eq!(press(&mut app, KeyCode::Char('q')), Action::Quit);
    }

    #[test]
    fn selection_follows_the_stream_across_refreshes() {
        let mut app = app();
        press(&mut app, KeyCode::Down);
        press(&mut app, KeyCode::Down);
        assert_eq!(app.selected_stream_id(), Some(303));
        // Firefox disappears: qbz must stay selected.
        let mut objects: Vec<serde_json::Value> = serde_json::from_str(DUMP).unwrap();
        objects.retain(|object| object["id"] != 300);
        let graph = Graph::from_pw_dump(&serde_json::to_string(&objects).unwrap()).unwrap();
        app.set_graph(graph);
        assert_eq!(app.selected_stream_id(), Some(303));
    }
}
