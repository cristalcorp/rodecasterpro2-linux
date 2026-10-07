//! What the Console panel shows: decided from the board service's answers,
//! pure, so it is tested without a service or a terminal.

use rcp2_audio::Channel;
use rcp2_proto::InputSource;

use crate::daemon::{DaemonError, StateDto};
use crate::{BOARD_BEING_READ, BOARD_NOT_CONNECTED};

/// Polls in a row that may fail for a passing reason while the last answer
/// is still shown: two that run out of time, or a service restarting (it
/// hangs up, then is absent for its systemd `RestartSec` of 2 s). Counted
/// rather than timed, so late polls cannot shorten it; every poll is bounded
/// by `QUERY_TIMEOUT`, so the wait stays bounded too.
const PASSING_POLLS: u32 = 3;

const NO_CHANNEL: &str = "No channel is assigned on the board.";
const ASKING: &str = "Asking the board service…";

/// Whether a failure may be waited out, keeping the last state for a moment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Patience {
    /// Said at once.
    None,
    /// The service is there but could not answer now.
    Passing,
    /// The service hung up: most often it is restarting.
    HungUp,
    /// No service: passing only after it hung up, while it restarts.
    Absent,
}

/// Why a poll failed, as shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoardIssue {
    pub(crate) text: String,
    pub(crate) patience: Patience,
}

impl From<&DaemonError> for BoardIssue {
    fn from(err: &DaemonError) -> Self {
        let patience = match err {
            DaemonError::Busy | DaemonError::NoAnswer => Patience::Passing,
            DaemonError::Dropped => Patience::HungUp,
            DaemonError::NotRunning => Patience::Absent,
            _ => Patience::None,
        };
        Self {
            text: sentence(&err.to_string()),
            patience,
        }
    }
}

/// An error message as a sentence: capital first letter, final period.
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    let mut out: String = chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default();
    if !out.ends_with(['.', ')', '…']) {
        out.push('.');
    }
    out
}

/// What a poll brought: the board state, or why there is none.
pub(crate) type BoardView = Result<StateDto, BoardIssue>;

/// One channel strip as shown (strip `n` sits on fader `n + 1`, verified on
/// hardware; strips past the last fader have none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Strip {
    pub(crate) fader: Option<usize>,
    pub(crate) source: String,
    pub(crate) code: Option<i32>,
    pub(crate) muted: Option<bool>,
    pub(crate) level: Option<i32>,
}

/// What the Console panel shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BoardShown {
    /// Strips worth showing (empty ones hidden, unreadable ones kept), and
    /// whether they may no longer be current.
    Strips {
        strips: Vec<Strip>,
        refreshing: bool,
    },
    Message(String),
}

impl BoardShown {
    /// The faders carrying a named output's channel, matched by source code.
    pub(crate) fn faders_of(&self, channel: Channel) -> Vec<usize> {
        let (Self::Strips { strips, .. }, Some(code)) =
            (self, channel_source(channel).and_then(InputSource::code))
        else {
            return Vec::new();
        };
        strips
            .iter()
            .filter(|strip| strip.code == Some(code))
            .filter_map(|strip| strip.fader)
            .collect()
    }
}

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

/// The board service's answers over time, and what they mean for the panel.
#[derive(Debug)]
pub(crate) struct Console {
    /// The last answer, while it still stands for the board: forgotten once
    /// a failure is said, so a later passing one cannot bring it back.
    last: Option<StateDto>,
    /// Polls failed in a row since the last answer.
    failed: u32,
    /// The service hung up since the last answer: it may be restarting.
    hung_up: bool,
    shown: BoardShown,
}

impl Console {
    pub(crate) fn new() -> Self {
        Self {
            last: None,
            failed: 0,
            hung_up: false,
            shown: BoardShown::Message(ASKING.to_owned()),
        }
    }

    pub(crate) const fn shown(&self) -> &BoardShown {
        &self.shown
    }

    /// Records what a poll brought, and decides the panel: a passing failure
    /// keeps the last answer for up to [`PASSING_POLLS`] polls, marked
    /// refreshing; anything else is said at once.
    pub(crate) fn record(&mut self, view: BoardView) {
        let issue = match view {
            Ok(state) => {
                self.shown = from_state(&state, false);
                self.last = Some(state);
                self.failed = 0;
                self.hung_up = false;
                return;
            }
            Err(issue) => issue,
        };
        self.failed = self.failed.saturating_add(1);
        self.hung_up |= issue.patience == Patience::HungUp;
        let passing = match issue.patience {
            Patience::None => false,
            Patience::Passing | Patience::HungUp => true,
            Patience::Absent => self.hung_up,
        };
        self.shown = match &self.last {
            Some(state) if passing && self.failed <= PASSING_POLLS => from_state(state, true),
            _ => {
                self.last = None;
                BoardShown::Message(issue.text)
            }
        };
    }
}

/// The panel for an answer; `unanswered` when later polls failed.
fn from_state(state: &StateDto, unanswered: bool) -> BoardShown {
    if !state.connected {
        return BoardShown::Message(BOARD_NOT_CONNECTED.to_owned());
    }
    if !state.state_known {
        return BoardShown::Message(BOARD_BEING_READ.to_owned());
    }
    let empty = InputSource::Empty.code();
    let strips: Vec<Strip> = state
        .channels
        .iter()
        .enumerate()
        .filter(|(_, channel)| channel.code != empty)
        .map(|(position, channel)| Strip {
            fader: (position < state.faders.len()).then_some(position + 1),
            source: channel.source.clone(),
            code: channel.code,
            muted: channel.muted,
            level: state.faders.get(position).copied().flatten(),
        })
        .collect();
    if strips.is_empty() {
        return BoardShown::Message(NO_CHANNEL.to_owned());
    }
    BoardShown::Strips {
        strips,
        refreshing: state.refreshing || unanswered,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{BoardIssue, BoardShown, Console, PASSING_POLLS, Patience};
    use crate::daemon::{ChannelDto, DaemonError, PROTOCOL_VERSION, StateDto};
    use rcp2_audio::Channel;

    /// A board with every kind of strip: muted, current, empty, unreadable,
    /// past the last fader.
    pub(crate) fn board_state(known: bool) -> StateDto {
        let strip = |index, source: &str, code, muted| ChannelDto {
            index,
            source: source.to_owned(),
            code,
            muted: Some(muted),
        };
        StateDto {
            version: PROTOCOL_VERSION,
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

    fn issue(err: &DaemonError) -> BoardIssue {
        BoardIssue::from(err)
    }

    fn failed(err: &DaemonError) -> super::BoardView {
        Err(issue(err))
    }

    fn refreshing(console: &Console) -> Option<bool> {
        match console.shown() {
            BoardShown::Strips { refreshing, .. } => Some(*refreshing),
            BoardShown::Message(_) => None,
        }
    }

    fn text(console: &Console) -> &str {
        match console.shown() {
            BoardShown::Message(text) => text,
            BoardShown::Strips { .. } => "",
        }
    }

    #[test]
    fn errors_read_as_sentences_and_say_whether_to_wait() {
        let older = issue(&DaemonError::ServiceOlder);
        assert!(
            older
                .text
                .starts_with("The board service is older than this rcp2ctl")
        );
        assert!(older.text.contains("`rcp2ctl hid setup`"));
        let not_running = issue(&DaemonError::NotRunning);
        assert!(
            not_running
                .text
                .contains("`rcp2ctl hid setup` installs and starts it")
        );
        assert!(not_running.text.ends_with(')'));
        assert_eq!(older.patience, Patience::None);
        assert_eq!(not_running.patience, Patience::Absent);
        assert_eq!(issue(&DaemonError::Busy).patience, Patience::Passing);
        assert_eq!(issue(&DaemonError::NoAnswer).patience, Patience::Passing);
        assert_eq!(issue(&DaemonError::Dropped).patience, Patience::HungUp);
        assert_eq!(
            issue(&DaemonError::BadAnswer("x".to_owned())).text,
            "The board service sent an invalid answer: x."
        );
    }

    #[test]
    fn passing_failures_keep_the_last_state_for_a_while() {
        let mut console = Console::new();
        console.record(Ok(board_state(true)));
        assert_eq!(refreshing(&console), Some(false));
        let passing = [
            DaemonError::NoAnswer,
            DaemonError::Dropped,
            DaemonError::Busy,
        ];
        assert_eq!(passing.len(), PASSING_POLLS as usize);
        for err in &passing {
            console.record(failed(err));
            assert_eq!(refreshing(&console), Some(true), "{err}");
        }
        console.record(failed(&DaemonError::NoAnswer));
        assert_eq!(text(&console), "The board service did not answer in time.");
        console.record(Ok(board_state(true)));
        assert_eq!(refreshing(&console), Some(false));
    }

    #[test]
    fn a_service_restarting_keeps_the_last_state() {
        let mut console = Console::new();
        console.record(Ok(board_state(true)));
        // It hangs up, then is absent until systemd starts it again.
        console.record(failed(&DaemonError::Dropped));
        console.record(failed(&DaemonError::NotRunning));
        console.record(failed(&DaemonError::NotRunning));
        assert_eq!(refreshing(&console), Some(true));
        console.record(Ok(board_state(true)));
        assert_eq!(refreshing(&console), Some(false));
        // Absent with no hang-up first: stopped, said at once.
        console.record(failed(&DaemonError::NotRunning));
        assert!(text(&console).starts_with("The board service is not running"));
    }

    #[test]
    fn a_said_error_is_not_undone_by_a_passing_one() {
        let mut console = Console::new();
        console.record(Ok(board_state(true)));
        console.record(failed(&DaemonError::NotRunning));
        // The restarted service does not answer yet: the old strips stay gone.
        console.record(failed(&DaemonError::NoAnswer));
        assert_eq!(text(&console), "The board service did not answer in time.");
    }

    #[test]
    fn a_busy_service_keeps_a_message_too() {
        let mut console = Console::new();
        console.record(Ok(board_state(false)));
        console.record(failed(&DaemonError::Busy));
        assert_eq!(text(&console), crate::BOARD_BEING_READ);
    }

    #[test]
    fn a_lasting_failure_is_said_at_once() {
        let mut console = Console::new();
        console.record(Ok(board_state(true)));
        console.record(failed(&DaemonError::ServiceNewer));
        assert!(text(&console).starts_with("The board service is newer"));
        // A passing one with nothing to keep is said too.
        let mut console = Console::new();
        assert_eq!(text(&console), super::ASKING);
        console.record(failed(&DaemonError::Busy));
        assert_eq!(text(&console), "The board service is busy.");
    }

    #[test]
    fn the_service_refreshing_is_shown() {
        let mut console = Console::new();
        let mut state = board_state(true);
        state.refreshing = true;
        console.record(Ok(state));
        assert_eq!(refreshing(&console), Some(true));
    }

    #[test]
    fn says_why_there_are_no_strips() {
        let mut console = Console::new();
        let mut unplugged = board_state(false);
        unplugged.connected = false;
        console.record(Ok(unplugged));
        assert_eq!(text(&console), crate::BOARD_NOT_CONNECTED);
        let mut bare = board_state(true);
        bare.channels.clear();
        console.record(Ok(bare));
        assert_eq!(text(&console), super::NO_CHANNEL);
    }

    #[test]
    fn an_output_on_several_faders_lists_them_all() {
        let mut console = Console::new();
        let mut state = board_state(true);
        // Game on the strips of faders 2 and 5.
        if let Some(channel) = state.channels.get_mut(1) {
            channel.code = Some(12);
        }
        console.record(Ok(state));
        assert_eq!(console.shown().faders_of(Channel::Game), vec![2, 5]);
        assert!(console.shown().faders_of(Channel::A).is_empty());
    }
}
