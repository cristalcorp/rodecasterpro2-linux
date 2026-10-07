//! What the Console panel shows: decided from the board service's answers
//! and the time, pure, so it is tested without a service or a terminal.

use std::time::{Duration, Instant};

use rcp2_audio::Channel;
use rcp2_proto::InputSource;

use crate::daemon::{DaemonError, QUERY_TIMEOUT, StateDto};
use crate::{BOARD_BEING_READ, BOARD_NOT_CONNECTED};

/// How long the last answer is still shown when no newer one has come: one
/// poll interval, one poll that runs out of time, and one more as margin.
/// Past it the panel says the service does not answer, even if the poll
/// itself is stuck and never reports.
pub(crate) const ANSWER_GRACE: Duration = super::REFRESH_EVERY
    .saturating_add(QUERY_TIMEOUT)
    .saturating_add(QUERY_TIMEOUT);

const NO_CHANNEL: &str = "No channel is assigned on the board.";
const ASKING: &str = "Asking the board service…";

/// Why a poll failed, as shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BoardIssue {
    pub(crate) text: String,
    /// The service is there but could not answer now: the last state is
    /// worth keeping for a moment.
    pub(crate) transient: bool,
}

impl From<&DaemonError> for BoardIssue {
    fn from(err: &DaemonError) -> Self {
        Self {
            text: sentence(&err.to_string()),
            transient: err.is_transient(),
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
    /// The last answer, and when its poll ended.
    answer: Option<(Instant, StateDto)>,
    /// Why the polls since that answer failed, if they did.
    failure: Option<BoardIssue>,
    /// When the asking started.
    since: Instant,
    /// Whether the last answer was still recent at the last look.
    recent: bool,
    shown: BoardShown,
}

impl Console {
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            answer: None,
            failure: None,
            since: now,
            recent: true,
            shown: BoardShown::Message(ASKING.to_owned()),
        }
    }

    pub(crate) const fn shown(&self) -> &BoardShown {
        &self.shown
    }

    /// Records what a poll that ended at `ended` brought.
    pub(crate) fn record(&mut self, view: BoardView, ended: Instant) {
        match view {
            Ok(state) => {
                self.answer = Some((ended, state));
                self.failure = None;
            }
            Err(issue) => self.failure = Some(issue),
        }
        self.recent = self.is_recent(ended);
        self.shown = self.decide();
    }

    /// Looks at the time: an answer too old means the service no longer
    /// answers, even when no poll reports it. Cheap when nothing changes.
    pub(crate) fn tick(&mut self, now: Instant) {
        let recent = self.is_recent(now);
        if recent != self.recent {
            self.recent = recent;
            self.shown = self.decide();
        }
    }

    fn is_recent(&self, now: Instant) -> bool {
        let last = self.answer.as_ref().map_or(self.since, |(at, _)| *at);
        now.saturating_duration_since(last) < ANSWER_GRACE
    }

    fn decide(&self) -> BoardShown {
        let message = |text: &str| BoardShown::Message(text.to_owned());
        match (&self.failure, &self.answer) {
            // A lasting failure, or one not worth waiting through, is said.
            (Some(issue), _) if !issue.transient || !self.recent => message(&issue.text),
            (Some(issue), None) => message(&issue.text),
            (failure, Some((_, state))) if self.recent => from_state(state, failure.is_some()),
            (None, None) if self.recent => message(ASKING),
            // Nothing for too long: the poll itself is stuck.
            _ => BoardShown::Message(sentence(&DaemonError::NoAnswer.to_string())),
        }
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
    use super::{ANSWER_GRACE, BoardIssue, BoardShown, Console};
    use crate::daemon::{ChannelDto, DaemonError, PROTOCOL_VERSION, QUERY_TIMEOUT, StateDto};
    use crate::tui::REFRESH_EVERY;
    use rcp2_audio::Channel;
    use std::time::Instant;

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
        assert!(!older.transient);
        assert!(issue(&DaemonError::Busy).transient);
        assert!(issue(&DaemonError::NoAnswer).transient);
        assert_eq!(
            issue(&DaemonError::BadAnswer("x".to_owned())).text,
            "The board service sent an invalid answer: x."
        );
    }

    #[test]
    fn a_poll_that_runs_out_of_time_keeps_the_last_state_for_a_moment() {
        let start = Instant::now();
        let mut console = Console::new(start);
        console.record(Ok(board_state(true)), start);
        assert_eq!(refreshing(&console), Some(false));
        // A real poll that runs out of time ends this long after the answer.
        let timed_out = start + REFRESH_EVERY + QUERY_TIMEOUT;
        console.record(Err(issue(&DaemonError::NoAnswer)), timed_out);
        assert_eq!(refreshing(&console), Some(true));
        console.tick(start + ANSWER_GRACE);
        assert_eq!(text(&console), "The board service did not answer in time.");
        console.record(Ok(board_state(true)), start + ANSWER_GRACE);
        assert_eq!(refreshing(&console), Some(false));
    }

    #[test]
    fn a_busy_service_keeps_a_message_too() {
        let start = Instant::now();
        let mut console = Console::new(start);
        console.record(Ok(board_state(false)), start);
        console.record(Err(issue(&DaemonError::Busy)), start + REFRESH_EVERY);
        assert_eq!(text(&console), crate::BOARD_BEING_READ);
    }

    #[test]
    fn a_lasting_failure_is_said_at_once() {
        let start = Instant::now();
        let mut console = Console::new(start);
        console.record(Ok(board_state(true)), start);
        console.record(Err(issue(&DaemonError::ServiceNewer)), start);
        assert!(text(&console).starts_with("The board service is newer"));
    }

    #[test]
    fn a_stuck_poll_is_noticed_without_any_report() {
        let start = Instant::now();
        let mut console = Console::new(start);
        console.record(Ok(board_state(true)), start);
        // No poll ever reports again.
        console.tick(start + QUERY_TIMEOUT);
        assert_eq!(refreshing(&console), Some(false));
        console.tick(start + ANSWER_GRACE);
        assert_eq!(text(&console), "The board service did not answer in time.");
        // Same from the start, before any answer.
        let mut console = Console::new(start);
        assert_eq!(text(&console), super::ASKING);
        console.tick(start + ANSWER_GRACE);
        assert_eq!(text(&console), "The board service did not answer in time.");
    }

    #[test]
    fn the_service_refreshing_is_shown() {
        let start = Instant::now();
        let mut console = Console::new(start);
        let mut state = board_state(true);
        state.refreshing = true;
        console.record(Ok(state), start);
        assert_eq!(refreshing(&console), Some(true));
    }

    #[test]
    fn says_why_there_are_no_strips() {
        let start = Instant::now();
        let mut console = Console::new(start);
        let mut unplugged = board_state(false);
        unplugged.connected = false;
        console.record(Ok(unplugged), start);
        assert_eq!(text(&console), crate::BOARD_NOT_CONNECTED);
        let mut bare = board_state(true);
        bare.channels.clear();
        console.record(Ok(bare), start);
        assert_eq!(text(&console), super::NO_CHANNEL);
    }

    #[test]
    fn an_output_on_several_faders_lists_them_all() {
        let start = Instant::now();
        let mut console = Console::new(start);
        let mut state = board_state(true);
        // Game on the strips of faders 2 and 5.
        if let Some(channel) = state.channels.get_mut(1) {
            channel.code = Some(12);
        }
        console.record(Ok(state), start);
        assert_eq!(console.shown().faders_of(Channel::Game), vec![2, 5]);
        assert!(console.shown().faders_of(Channel::A).is_empty());
    }
}
