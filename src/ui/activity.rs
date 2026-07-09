//! Per-session turn tracking: is the agent working on a command, for how
//! long, and did it finish while the user wasn't looking (unread)?
//!
//! Signal model (no screen-scraping — it breaks on every agent release):
//! - An agent that is WORKING streams PTY output continuously (tokens,
//!   spinners, tool output); one waiting for input goes quiet.
//! - The user's own typing produces output too (echo, composer redraws), so
//!   output within an echo-grace window of the last input write does not
//!   start a turn. A submitted command keeps streaming long after the
//!   window, which is when the turn becomes visible — its start time is
//!   backdated to the submitting keystroke.
//! - When the stream goes quiet the turn is DONE; if the session wasn't the
//!   one on screen at that moment it is also UNREAD until viewed.
//!
//! The tracker is a pure state machine fed by the app tick so it can be
//! unit-tested with synthetic clocks.

use std::time::{Duration, Instant};

/// Output younger than this counts as "still streaming".
pub const STREAM_WINDOW: Duration = Duration::from_secs(3);
/// Output within this window of the user's last keystroke is presumed echo,
/// not agent work.
pub const ECHO_GRACE: Duration = Duration::from_secs(3);
/// A turn's start is backdated to the last input if it was at most this old
/// when the stream was first classified as work.
const BACKDATE_MAX: Duration = Duration::from_secs(15);
/// Attaching/resuming a session makes the agent repaint its whole TUI —
/// a burst of output with no preceding input that must NOT read as work.
/// Within this window after a session opens, only a user-submitted command
/// starts a turn; spontaneous output is treated as the attach redraw
/// (genuine ongoing work keeps streaming past the window and starts then).
/// Comfortably longer than a redraw burst (~1s) plus STREAM_WINDOW.
pub const SETTLE_WINDOW: Duration = Duration::from_secs(5);
/// Output younger than this counts toward ACTIVE working time (tighter
/// than STREAM_WINDOW, which only decides whether the turn is still open —
/// idle-keepalive blips must not inflate the visible timer).
const ACTIVE_WINDOW: Duration = Duration::from_secs(1);
/// Cap per-observation accumulation so a stalled tick thread can't credit
/// a huge jump in one step.
const OBS_DELTA_MAX: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Turn {
    /// No turn observed yet (fresh runtime) or last turn was read.
    Idle,
    /// The agent is working on a command.
    Working { since: Instant },
    /// The last turn finished; `unread` until the session is viewed.
    Done { finished: Instant, unread: bool },
}

#[derive(Debug, Clone, Copy)]
pub struct Activity {
    pub turn: Turn,
    /// When this tracker (and thus the session pane) began — the anchor for
    /// the settle window. Set at first construction/observe.
    created: Instant,
    /// Accumulated time the agent actually spent PRODUCING output during
    /// the current turn (what the "working Xm" timer shows) — not
    /// wall-clock since the turn began: waiting on approvals or idle
    /// keepalives must not count.
    active: Duration,
    /// Previous observe() instant, for delta accumulation.
    last_obs: Option<Instant>,
}

impl Default for Activity {
    fn default() -> Self {
        Activity {
            turn: Turn::Idle,
            created: Instant::now(),
            active: Duration::ZERO,
            last_obs: None,
        }
    }
}

impl Activity {
    /// Time the agent spent actively producing output in the current turn
    /// (frozen when the turn ends).
    pub fn active_time(&self) -> Duration {
        self.active
    }

    /// Feed one observation. `last_output_age`/`last_input_age` are the ages
    /// of the most recent PTY output / user input write (None = never);
    /// `viewed` = this session's pane is what the user is looking at.
    pub fn observe(
        &mut self,
        now: Instant,
        last_output_age: Duration,
        last_input_age: Option<Duration>,
        viewed: bool,
    ) {
        let delta = self
            .last_obs
            .map(|t| now.saturating_duration_since(t))
            .unwrap_or(Duration::ZERO)
            .min(OBS_DELTA_MAX);
        self.last_obs = Some(now);
        let streaming = last_output_age < STREAM_WINDOW;
        let input_recent = last_input_age.map(|a| a < ECHO_GRACE).unwrap_or(false);
        // Credit active time only while a turn is open AND output is
        // genuinely flowing right now.
        if matches!(self.turn, Turn::Working { .. }) && last_output_age < ACTIVE_WINDOW {
            self.active += delta;
        }
        // A command the user submitted in this vag session: input exists,
        // past the echo window, still plausibly this turn.
        let submitted = matches!(last_input_age, Some(a) if a >= ECHO_GRACE && a <= BACKDATE_MAX);
        // Past the settle window, sustained streaming is genuine work even
        // without an in-vag keystroke (e.g. a session resumed mid-task).
        let settled = now.saturating_duration_since(self.created) >= SETTLE_WINDOW;
        match self.turn {
            Turn::Idle | Turn::Done { .. } => {
                if streaming && !input_recent && (submitted || settled) {
                    // Backdate to the submitting keystroke when plausible.
                    let since = match last_input_age {
                        Some(a) if a <= BACKDATE_MAX => now - a,
                        _ => now - last_output_age,
                    };
                    self.turn = Turn::Working { since };
                    // Seed with the streaming that led to classification
                    // (bounded by the backdate window).
                    self.active = now.saturating_duration_since(since).min(BACKDATE_MAX);
                } else if let Turn::Done {
                    finished,
                    unread: true,
                } = self.turn
                    && viewed
                {
                    self.turn = Turn::Done {
                        finished,
                        unread: false,
                    };
                }
            }
            Turn::Working { since } => {
                if !streaming {
                    self.turn = Turn::Done {
                        finished: now - last_output_age,
                        unread: !viewed,
                    };
                } else {
                    // keep `since` stable across the whole turn
                    self.turn = Turn::Working { since };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    fn base() -> Instant {
        Instant::now()
    }

    #[test]
    fn active_time_counts_streaming_not_wall_clock() {
        let mut a = Activity::default();
        let t0 = base();
        // Turn starts (submitted 5s ago, streaming) — seed = 5s.
        a.observe(t0, Duration::ZERO, Some(5 * S), true);
        assert!(matches!(a.turn, Turn::Working { .. }));
        assert_eq!(a.active_time(), 5 * S);
        // 3 ticks of genuine streaming (fresh output) → +3s.
        for i in 1..=3u32 {
            a.observe(t0 + i * S, Duration::ZERO, None, true);
        }
        assert_eq!(a.active_time(), 8 * S);
        // Quiet-ish keepalive phase: output 2s old keeps the TURN open
        // (< STREAM_WINDOW) but must NOT accrue active time.
        for i in 4..=10u32 {
            a.observe(t0 + i * S, 2 * S, None, true);
        }
        assert!(matches!(a.turn, Turn::Working { .. }), "turn stays open");
        assert_eq!(a.active_time(), 8 * S, "keepalives don't inflate");
        // Streaming resumes → accrues again.
        a.observe(t0 + 11 * S, Duration::ZERO, None, true);
        assert_eq!(a.active_time(), 9 * S);
        // Turn ends: active time freezes.
        a.observe(t0 + 16 * S, 4 * S, None, true);
        assert!(matches!(a.turn, Turn::Done { .. }));
        assert_eq!(a.active_time(), 9 * S);
    }

    #[test]
    fn typing_alone_never_starts_a_turn() {
        let mut a = Activity::default();
        let t0 = base();
        // user typing: output fresh, input fresh
        a.observe(t0, Duration::ZERO, Some(Duration::ZERO), true);
        a.observe(t0 + S, S / 2, Some(S / 2), true);
        assert_eq!(a.turn, Turn::Idle);
    }

    #[test]
    fn submit_then_stream_starts_turn_backdated_to_input() {
        let mut a = Activity::default();
        let t0 = base();
        // 5s after the enter keypress the agent is still streaming
        a.observe(t0, Duration::ZERO, Some(5 * S), true);
        match a.turn {
            Turn::Working { since } => assert_eq!(since, t0 - 5 * S),
            other => panic!("expected working, got {other:?}"),
        }
    }

    #[test]
    fn attach_redraw_burst_does_not_start_a_turn() {
        // The reported bug: opening an idle session repaints the whole TUI
        // (burst of output, no input). It must NOT read as working.
        let mut a = Activity::default();
        let t0 = base();
        // Redraw burst over the first ~1s, then the agent sits quiet.
        a.observe(t0, Duration::ZERO, None, true);
        a.observe(t0 + S / 2, Duration::ZERO, None, true);
        a.observe(t0 + S, Duration::ZERO, None, true);
        assert_eq!(a.turn, Turn::Idle, "attach redraw wrongly flagged working");
        // By the time the settle window passes the burst is long over, so
        // the output is stale and the session stays idle.
        a.observe(t0 + 6 * S, 5 * S, None, true);
        assert_eq!(a.turn, Turn::Idle);
    }

    #[test]
    fn sustained_output_past_settle_window_starts_turn() {
        // A session resumed mid-task keeps streaming — genuine work, marked
        // once it outlasts the attach-settle window.
        let mut a = Activity::default();
        let t0 = base();
        a.observe(t0, S, None, false); // within settle: not yet
        assert_eq!(a.turn, Turn::Idle);
        a.observe(t0 + 6 * S, S, None, false); // still streaming after settle
        assert!(matches!(a.turn, Turn::Working { .. }));
    }

    #[test]
    fn submitted_command_starts_turn_even_within_settle() {
        // A command you send right after attaching is real work immediately
        // (submitted-input path bypasses the settle window).
        let mut a = Activity::default();
        let t0 = base();
        // input 4s ago (past echo grace), output still streaming, 4s < settle
        a.observe(t0 + 4 * S, Duration::ZERO, Some(4 * S), true);
        assert!(matches!(a.turn, Turn::Working { .. }));
    }

    #[test]
    fn quiet_stream_finishes_turn_unread_when_not_viewed() {
        let mut a = Activity::default();
        let t0 = base();
        a.observe(t0, Duration::ZERO, Some(5 * S), false);
        assert!(matches!(a.turn, Turn::Working { .. }));
        // stream stopped 4s ago -> done, unread
        a.observe(t0 + 10 * S, 4 * S, Some(15 * S), false);
        match a.turn {
            Turn::Done { unread, finished } => {
                assert!(unread);
                assert_eq!(finished, t0 + 6 * S);
            }
            other => panic!("expected done, got {other:?}"),
        }
    }

    #[test]
    fn finishing_while_viewed_is_already_read() {
        let mut a = Activity::default();
        let t0 = base();
        a.observe(t0, Duration::ZERO, Some(5 * S), true);
        a.observe(t0 + 10 * S, 4 * S, Some(15 * S), true);
        assert!(matches!(a.turn, Turn::Done { unread: false, .. }));
    }

    #[test]
    fn viewing_clears_unread() {
        let mut a = Activity::default();
        let t0 = base();
        a.observe(t0, Duration::ZERO, Some(5 * S), false);
        a.observe(t0 + 10 * S, 4 * S, None, false);
        assert!(matches!(a.turn, Turn::Done { unread: true, .. }));
        a.observe(t0 + 12 * S, 6 * S, None, true);
        assert!(matches!(a.turn, Turn::Done { unread: false, .. }));
    }

    #[test]
    fn working_since_is_stable_across_the_turn() {
        let mut a = Activity::default();
        let t0 = base();
        a.observe(t0, Duration::ZERO, Some(4 * S), false);
        let Turn::Working { since } = a.turn else {
            panic!()
        };
        a.observe(t0 + 30 * S, Duration::ZERO, None, false);
        let Turn::Working { since: since2 } = a.turn else {
            panic!()
        };
        assert_eq!(since, since2);
    }

    #[test]
    fn new_turn_after_done_resets_timer() {
        let mut a = Activity::default();
        let t0 = base();
        a.observe(t0, Duration::ZERO, Some(5 * S), true);
        a.observe(t0 + 10 * S, 5 * S, None, true); // done, read
        // fresh input then stream again
        a.observe(t0 + 20 * S, Duration::ZERO, Some(4 * S), true);
        match a.turn {
            Turn::Working { since } => assert_eq!(since, t0 + 16 * S),
            other => panic!("expected working, got {other:?}"),
        }
    }
}
