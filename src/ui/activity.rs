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
//! - Provider-native start/resume events establish the visible in-flight
//!   phase immediately. PTY output still measures persisted active-output
//!   time and supplies the fallback completion boundary when a provider has
//!   no safe post-turn event.
//! - When the stream goes quiet the turn is DONE; if the session wasn't the
//!   one on screen at that moment it is also UNREAD until viewed.
//!
//! The tracker is a pure state machine fed by the app tick so it can be
//! unit-tested with synthetic clocks.

use std::time::{Duration, Instant};

use crate::agent_events::{AgentEvent, AgentEventKind, NeedsInputKind};

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

#[derive(Debug, Clone)]
struct Attention {
    kind: NeedsInputKind,
    since: Instant,
    unread: bool,
    request_id: Option<String>,
    observed_at_unix_nanos: u64,
}

/// A provider-confirmed prompt/resume phase. This is intentionally separate
/// from `Turn::Working`: the native phase supplies precise sidebar timing,
/// while the PTY turn continues to own active-output accounting and fallback
/// completion detection.
#[derive(Debug, Clone)]
struct NativeWorking {
    since: Instant,
    /// True only after a provider event confirms the phase. Local submit
    /// and mouse fallbacks make the UI responsive but cannot safely tighten
    /// persisted accounting until a native boundary corroborates them.
    provider_confirmed: bool,
    /// Becomes true once PTY output newer than `since` is observed. After
    /// that, normal stream silence is a safe fallback end boundary for
    /// providers (notably Claude) without a non-blockable immediate Stop.
    output_seen: bool,
}

/// Read-only semantic attention state exposed to renderers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttentionView {
    pub kind: NeedsInputKind,
    pub since: Instant,
    pub unread: bool,
}

#[derive(Debug, Clone)]
pub struct Activity {
    pub turn: Turn,
    /// When this tracker (and thus the session pane) began — the anchor for
    /// the settle window. Set at first construction/observe.
    created: Instant,
    /// Accumulated time the agent actually spent PRODUCING output during
    /// the current turn — not wall-clock since the turn began: waiting on
    /// approvals or idle keepalives must not count. This feeds persisted
    /// stats; the sidebar's in-flight clock comes from `working_since()`.
    /// Resets to a fresh seed at the start of each new turn (see `credited`
    /// for the lifetime-monotonic total).
    active: Duration,
    /// Lifetime total of active time ever recognized by this tracker,
    /// across every turn — unlike `active` this NEVER resets when a turn
    /// ends and a new one begins. Callers (persisted daily stats) diff
    /// consecutive values via `observe()`'s return to credit newly
    /// recognized time exactly once, regardless of how `active` itself is
    /// reset or backdated turn-to-turn.
    credited: Duration,
    /// Previous observe() instant, for delta accumulation.
    last_obs: Option<Instant>,
    /// Provider-native input/approval state. This overlays (rather than
    /// replaces) the PTY heuristic so sessions without native signals keep
    /// their historical behavior.
    attention: Option<Attention>,
    /// Provider-confirmed prompt/resume phase used by the visible sidebar
    /// clock. PTY observations still own persisted active-time accounting.
    native_working: Option<NativeWorking>,
    /// A native completion reached the normal composer. This is an internal
    /// next-submit boundary only; once the session is viewed, Done disappears
    /// from the UI while this latch remains available to restart the
    /// in-flight clock even though Codex exposes no turn-start event.
    native_ready: bool,
    /// Latest accepted lifecycle/input boundary, comparable with hook
    /// process timestamps so a delayed packet cannot regress the state.
    last_transition_unix_nanos: u64,
    /// A mouse interaction forwarded while attention was latched. A click
    /// alone is not resolution, but sustained output after it is a safe PTY
    /// fallback for providers that expose no native approval-resumed event.
    pending_interaction_unix_nanos: u64,
}

impl Default for Activity {
    fn default() -> Self {
        Activity {
            turn: Turn::Idle,
            created: Instant::now(),
            active: Duration::ZERO,
            credited: Duration::ZERO,
            last_obs: None,
            attention: None,
            native_working: None,
            native_ready: false,
            last_transition_unix_nanos: 0,
            pending_interaction_unix_nanos: 0,
        }
    }
}

impl Activity {
    /// Logical start of the phase currently rendered as Working. Native
    /// provider timing wins; PTY-only/custom sessions retain the heuristic
    /// `Turn::Working` start.
    pub fn working_since(&self) -> Option<Instant> {
        if self.attention.is_some() {
            return None;
        }
        let heuristic = if let Turn::Working { since } = self.turn {
            Some(since)
        } else {
            None
        };
        self.native_working
            .as_ref()
            .map(|working| working.since)
            .or(heuristic)
    }

    /// Mark all visible state as read as soon as its pane is attached. The
    /// semantic wait/native-ready latches remain intact; only unread UI is
    /// cleared. Returns whether a repaint-relevant bit changed.
    pub fn mark_viewed(&mut self) -> bool {
        let mut changed = false;
        if let Some(attention) = self.attention.as_mut()
            && attention.unread
        {
            attention.unread = false;
            changed = true;
        }
        if let Turn::Done {
            finished,
            unread: true,
        } = self.turn
        {
            self.turn = Turn::Done {
                finished,
                unread: false,
            };
            changed = true;
        }
        changed
    }

    pub fn attention(&self) -> Option<AttentionView> {
        self.attention.as_ref().map(|a| AttentionView {
            kind: a.kind,
            since: a.since,
            unread: a.unread,
        })
    }

    /// Apply an agent-native lifecycle transition. `now` is the receiving
    /// process's monotonic clock (for durations); the event's Unix timestamp
    /// is used only to reject stale cross-process packets.
    pub fn on_agent_event(&mut self, event: AgentEvent, now: Instant, viewed: bool) {
        match event.kind {
            AgentEventKind::NeedsInput { kind, request_id } => {
                if event.observed_at_unix_nanos <= self.last_transition_unix_nanos {
                    return;
                }
                if let Some(current) = self.attention.as_mut() {
                    if event.observed_at_unix_nanos < current.observed_at_unix_nanos {
                        return;
                    }
                    // Generic idle/ready signals are delayed fallbacks. They
                    // must never erase a more specific live wait (or reset a
                    // different generic wait's age).
                    if matches!(kind, NeedsInputKind::Input | NeedsInputKind::NextPrompt)
                        && current.kind != kind
                    {
                        return;
                    }
                    let same_request = current.request_id == request_id
                        || current.request_id.is_none()
                        || request_id.is_none();
                    if current.kind == kind && same_request {
                        current.observed_at_unix_nanos = event.observed_at_unix_nanos;
                        if current.request_id.is_none() {
                            current.request_id = request_id;
                        }
                        return;
                    }
                }
                // Freeze heuristic working time immediately. The attention
                // overlay owns the visible state until input/resolution.
                self.native_working = None;
                self.native_ready = false;
                self.turn = Turn::Done {
                    finished: now,
                    unread: false,
                };
                self.pending_interaction_unix_nanos = 0;
                self.attention = Some(Attention {
                    kind,
                    since: now,
                    unread: !viewed,
                    request_id,
                    observed_at_unix_nanos: event.observed_at_unix_nanos,
                });
            }
            AgentEventKind::InputResolved { request_id } => {
                if event.observed_at_unix_nanos <= self.last_transition_unix_nanos {
                    return;
                }
                let accepted = self.attention.as_ref().is_none_or(|current| {
                    event.observed_at_unix_nanos >= current.observed_at_unix_nanos
                        && (request_id.is_none()
                            || current.request_id.is_none()
                            || current.request_id == request_id)
                });
                if !accepted {
                    return;
                }
                let begins_phase = self.attention.is_some() || self.native_working.is_none();
                self.last_transition_unix_nanos = event.observed_at_unix_nanos;
                self.attention = None;
                self.pending_interaction_unix_nanos = 0;
                if begins_phase {
                    self.start_native_working(now, true);
                } else if let Some(native) = self.native_working.as_mut()
                    && !native.provider_confirmed
                {
                    native.provider_confirmed = true;
                    self.turn = Turn::Idle;
                    self.active = Duration::ZERO;
                    self.last_obs = Some(now);
                }
            }
            AgentEventKind::TurnStarted => {
                if event.observed_at_unix_nanos <= self.last_transition_unix_nanos
                    || self.attention.as_ref().is_some_and(|current| {
                        event.observed_at_unix_nanos < current.observed_at_unix_nanos
                    })
                {
                    return;
                }
                self.last_transition_unix_nanos = event.observed_at_unix_nanos;
                self.pending_interaction_unix_nanos = 0;
                self.native_ready = false;
                if let Some(native) = self.native_working.as_mut()
                    && !native.provider_confirmed
                {
                    // A local submit already gave the UI the earlier, more
                    // precise start. Upgrade it instead of resetting the
                    // visible clock when Claude's hook arrives milliseconds
                    // later.
                    native.provider_confirmed = true;
                    self.turn = Turn::Idle;
                    self.active = Duration::ZERO;
                    self.last_obs = Some(now);
                } else {
                    self.start_native_working(now, true);
                }
            }
            AgentEventKind::TurnCompleted => {
                if event.observed_at_unix_nanos <= self.last_transition_unix_nanos
                    || self.attention.as_ref().is_some_and(|current| {
                        event.observed_at_unix_nanos < current.observed_at_unix_nanos
                    })
                {
                    return;
                }
                self.last_transition_unix_nanos = event.observed_at_unix_nanos;
                self.pending_interaction_unix_nanos = 0;
                self.attention = None;
                self.native_working = None;
                self.native_ready = true;
                self.turn = Turn::Done {
                    finished: now,
                    unread: !viewed,
                };
            }
            AgentEventKind::SessionEnded => {
                if event.observed_at_unix_nanos <= self.last_transition_unix_nanos
                    || self.attention.as_ref().is_some_and(|current| {
                        event.observed_at_unix_nanos < current.observed_at_unix_nanos
                    })
                {
                    return;
                }
                self.last_transition_unix_nanos = event.observed_at_unix_nanos;
                self.pending_interaction_unix_nanos = 0;
                self.attention = None;
                self.native_working = None;
                self.native_ready = false;
                self.turn = Turn::Idle;
            }
        }
    }

    fn record_user_boundary(&mut self, observed_at_unix_nanos: u64) -> bool {
        self.last_transition_unix_nanos =
            self.last_transition_unix_nanos.max(observed_at_unix_nanos);
        let should_clear = self
            .attention
            .as_ref()
            .is_some_and(|attention| attention.observed_at_unix_nanos <= observed_at_unix_nanos);
        if should_clear {
            self.attention = None;
        }
        should_clear
    }

    /// A person submitted the currently waiting interaction. This is a
    /// reliable resume boundary (unlike partial typing), so the sidebar can
    /// return to Working before PTY output arrives.
    pub fn on_user_submit(&mut self, observed_at_unix_nanos: u64, now: Instant) {
        let resumes_attention = self.record_user_boundary(observed_at_unix_nanos);
        if resumes_attention || self.native_ready {
            self.native_ready = false;
            self.start_native_working(now, false);
        }
    }

    /// A person cancelled/dismissed the waiting interaction. Clear the latch
    /// without claiming that agent work resumed.
    pub fn on_user_cancel(&mut self, observed_at_unix_nanos: u64) {
        if self.record_user_boundary(observed_at_unix_nanos) {
            self.native_working = None;
            self.turn = Turn::Idle;
        }
    }

    /// Record a possible mouse-based resolution without clearing attention.
    /// A later sustained-output observation promotes it to an actual resume.
    pub fn on_user_interaction(&mut self, observed_at_unix_nanos: u64) {
        self.pending_interaction_unix_nanos = self
            .pending_interaction_unix_nanos
            .max(observed_at_unix_nanos);
    }

    fn start_native_working(&mut self, now: Instant, provider_confirmed: bool) {
        self.start_native_working_at(now, now, provider_confirmed);
    }

    fn start_native_working_at(
        &mut self,
        since: Instant,
        observed_now: Instant,
        provider_confirmed: bool,
    ) {
        self.attention = None;
        self.native_ready = false;
        self.native_working = Some(NativeWorking {
            since,
            provider_confirmed,
            output_seen: false,
        });
        // The heuristic starts a fresh accounting segment once genuine
        // output survives echo grace. Lifetime `credited` stays monotonic.
        self.turn = Turn::Idle;
        self.active = Duration::ZERO;
        self.last_obs = Some(observed_now);
    }

    /// Feed one observation. `last_output_age`/`last_input_age` are the ages
    /// of the most recent PTY output / user input write (None = never);
    /// `viewed` = this session's pane is what the user is looking at.
    ///
    /// Returns the active time newly recognized by THIS call (usually
    /// `Duration::ZERO`) — callers accumulate this into persisted
    /// day-bucketed stats so historical totals survive restarts without
    /// double-counting across turn boundaries.
    pub fn observe(
        &mut self,
        now: Instant,
        last_output_age: Duration,
        last_input_age: Option<Duration>,
        viewed: bool,
    ) -> Duration {
        let credited_before = self.credited;
        let delta = self
            .last_obs
            .map(|t| now.saturating_duration_since(t))
            .unwrap_or(Duration::ZERO)
            .min(OBS_DELTA_MAX);
        self.last_obs = Some(now);
        let streaming = last_output_age < STREAM_WINDOW;
        let input_recent = last_input_age.map(|a| a < ECHO_GRACE).unwrap_or(false);
        // A command the user submitted in this vag session: input exists,
        // past the echo window, still plausibly this turn.
        let submitted = matches!(last_input_age, Some(a) if a >= ECHO_GRACE && a <= BACKDATE_MAX);
        if let Some(attention) = self.attention.as_mut() {
            if viewed {
                attention.unread = false;
            }
            let mouse_resume = self.pending_interaction_unix_nanos
                > attention.observed_at_unix_nanos
                && submitted
                && streaming;
            if mouse_resume {
                // A click by itself may only select an option. Sustained PTY
                // output beyond echo grace proves the child actually resumed.
                self.last_transition_unix_nanos = self
                    .last_transition_unix_nanos
                    .max(self.pending_interaction_unix_nanos);
                self.pending_interaction_unix_nanos = 0;
                self.attention = None;
                let since = now - last_input_age.unwrap_or_default();
                self.start_native_working_at(since, now, false);
                if let Some(native) = self.native_working.as_mut() {
                    native.output_seen = true;
                }
            } else {
                // Prompt/approval repaints can be noisy. A native attention
                // latch is authoritative and must neither restart the
                // heuristic turn nor accrue active-working time.
                return Duration::ZERO;
            }
        }
        if self.native_ready {
            // The completion notification itself repaints the PTY and can
            // look like fresh streaming. Hold the exact Done boundary until
            // the next real submit instead of letting that repaint restart
            // the heuristic turn.
            if viewed
                && let Turn::Done {
                    finished,
                    unread: true,
                } = self.turn
            {
                self.turn = Turn::Done {
                    finished,
                    unread: false,
                };
            }
            return Duration::ZERO;
        }
        if let Some(native) = self.native_working.as_mut() {
            let output_after_native = last_output_age < now.saturating_duration_since(native.since);
            let output_after_input = last_input_age.is_some_and(|age| last_output_age < age);
            if output_after_native || output_after_input {
                native.output_seen = true;
            }
        }
        // Credit active time only while a turn is open AND output is
        // genuinely flowing right now. A provider-confirmed native start can
        // account immediately; PTY-only sessions wait for heuristic
        // classification exactly as before.
        let native_active = self
            .native_working
            .as_ref()
            .is_some_and(|native| native.provider_confirmed && native.output_seen);
        if (native_active || matches!(self.turn, Turn::Working { .. }))
            && last_output_age < ACTIVE_WINDOW
        {
            self.active += delta;
            self.credited += delta;
        }
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
                    if !self
                        .native_working
                        .as_ref()
                        .is_some_and(|native| native.provider_confirmed)
                    {
                        // PTY-only fallback cannot see the precise start, so
                        // seed with the streaming that led to classification
                        // (bounded by the historical backdate window). A
                        // provider-confirmed phase has already accumulated
                        // fresh-output deltas tick by tick and must not seed
                        // or double-count silent startup latency.
                        let seed = now.saturating_duration_since(since).min(BACKDATE_MAX);
                        self.active = seed;
                        self.credited += seed;
                    }
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
        let native_finished = self.native_working.as_ref().is_some_and(|native| {
            (native.output_seen && !streaming)
                || (!native.output_seen
                    && now.saturating_duration_since(native.since) >= BACKDATE_MAX)
        });
        if native_finished {
            self.native_working = None;
            // A very short native turn can finish before echo grace ever
            // promotes the PTY heuristic to Working. Preserve its unread
            // completion instead of silently falling back to Idle.
            if matches!(self.turn, Turn::Idle) {
                self.turn = Turn::Done {
                    finished: now - last_output_age,
                    unread: !viewed,
                };
            }
        }
        self.credited - credited_before
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
        assert_eq!(a.active, 5 * S);
        // 3 ticks of genuine streaming (fresh output) → +3s.
        for i in 1..=3u32 {
            a.observe(t0 + i * S, Duration::ZERO, None, true);
        }
        assert_eq!(a.active, 8 * S);
        // Quiet-ish keepalive phase: output 2s old keeps the TURN open
        // (< STREAM_WINDOW) but must NOT accrue active time.
        for i in 4..=10u32 {
            a.observe(t0 + i * S, 2 * S, None, true);
        }
        assert!(matches!(a.turn, Turn::Working { .. }), "turn stays open");
        assert_eq!(a.active, 8 * S, "keepalives don't inflate");
        // Streaming resumes → accrues again.
        a.observe(t0 + 11 * S, Duration::ZERO, None, true);
        assert_eq!(a.active, 9 * S);
        // Turn ends: active time freezes.
        a.observe(t0 + 16 * S, 4 * S, None, true);
        assert!(matches!(a.turn, Turn::Done { .. }));
        assert_eq!(a.active, 9 * S);
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

    #[test]
    fn observe_returns_newly_credited_delta_summing_to_active_time() {
        let mut a = Activity::default();
        let t0 = base();
        // Turn starts backdated 5s — that whole seed is newly credited.
        let d0 = a.observe(t0, Duration::ZERO, Some(5 * S), true);
        assert_eq!(d0, 5 * S);
        assert_eq!(a.active, 5 * S);
        // 3 ticks of genuine streaming: each returns its own increment.
        let mut sum = d0;
        for i in 1..=3u32 {
            let d = a.observe(t0 + i * S, Duration::ZERO, None, true);
            assert_eq!(d, S, "tick {i} should credit exactly 1s");
            sum += d;
        }
        assert_eq!(sum, a.active);
        // Keepalive-only ticks (output 2s old) must not credit anything.
        for i in 4..=10u32 {
            let d = a.observe(t0 + i * S, 2 * S, None, true);
            assert_eq!(d, Duration::ZERO, "keepalive tick {i} must not credit");
        }
        // Turn ends: no further credit on the closing observation.
        let d = a.observe(t0 + 16 * S, 4 * S, None, true);
        assert_eq!(d, Duration::ZERO);
        assert!(matches!(a.turn, Turn::Done { .. }));
    }

    #[test]
    fn credited_delta_never_double_counts_across_a_new_turn() {
        // The regression this guards against: naively diffing `active`
        // (which RESETS to a fresh seed each new turn) instead of tracking
        // a monotonic lifetime total would either double-count or underflow
        // when a second turn starts with a smaller seed than the first
        // turn's final active total.
        let mut a = Activity::default();
        let t0 = base();
        // First turn: backdated 5s seed, runs 10s total (5s streamed).
        let mut total = a.observe(t0, Duration::ZERO, Some(5 * S), true);
        for i in 1..=5u32 {
            total += a.observe(t0 + i * S, Duration::ZERO, None, true);
        }
        assert_eq!(total, 10 * S);
        // Turn ends (quiet for 4s).
        total += a.observe(t0 + 15 * S, 4 * S, None, true);
        assert!(matches!(a.turn, Turn::Done { .. }));
        // Second turn starts with only a 4s backdated seed — smaller than
        // the first turn's 10s final `active`. Must credit exactly 4s more,
        // not underflow/panic and not re-credit the first turn's total.
        let d = a.observe(t0 + 20 * S, Duration::ZERO, Some(4 * S), true);
        assert_eq!(d, 4 * S);
        total += d;
        assert_eq!(total, 14 * S, "lifetime credited total across both turns");
    }

    fn needs(kind: NeedsInputKind, id: Option<&str>, at: u64) -> AgentEvent {
        AgentEvent {
            observed_at_unix_nanos: at,
            kind: AgentEventKind::NeedsInput {
                kind,
                request_id: id.map(str::to_string),
            },
        }
    }

    #[test]
    fn native_attention_freezes_work_and_stats_until_viewed_or_resolved() {
        let mut a = Activity::default();
        let t0 = base();
        a.observe(t0, Duration::ZERO, Some(5 * S), false);
        assert!(matches!(a.turn, Turn::Working { .. }));
        let active = a.active;

        a.on_agent_event(
            needs(NeedsInputKind::Approval, Some("p1"), 100),
            t0 + S,
            false,
        );
        let waiting = a.attention().expect("approval latched");
        assert_eq!(waiting.kind, NeedsInputKind::Approval);
        assert!(waiting.unread);
        assert!(matches!(a.turn, Turn::Done { unread: false, .. }));

        // Even noisy prompt repaint output cannot restart/credit work.
        let credited = a.observe(t0 + 2 * S, Duration::ZERO, None, false);
        assert_eq!(credited, Duration::ZERO);
        assert_eq!(a.active, active);
        // Viewing acknowledges unread without clearing the actual wait.
        a.observe(t0 + 3 * S, Duration::ZERO, None, true);
        assert!(!a.attention().unwrap().unread);
    }

    #[test]
    fn native_request_dedup_resolution_and_specificity_are_stable() {
        let mut a = Activity::default();
        let t0 = base();
        a.on_agent_event(needs(NeedsInputKind::Question, Some("q1"), 100), t0, false);
        let since = a.attention().unwrap().since;
        // Fallback notification for the same question keeps the original age.
        a.on_agent_event(
            needs(NeedsInputKind::Question, None, 110),
            t0 + 5 * S,
            false,
        );
        assert_eq!(a.attention().unwrap().since, since);
        // Delayed idle_prompt must not erase a specific question.
        a.on_agent_event(
            needs(NeedsInputKind::NextPrompt, None, 120),
            t0 + 6 * S,
            false,
        );
        assert_eq!(a.attention().unwrap().kind, NeedsInputKind::Question);
        a.on_agent_event(
            needs(NeedsInputKind::Input, None, 125),
            t0 + 6 * S + S / 2,
            false,
        );
        assert_eq!(
            a.attention().unwrap().kind,
            NeedsInputKind::Question,
            "a delayed generic idle notification cannot downgrade a specific wait"
        );

        a.on_agent_event(
            AgentEvent {
                observed_at_unix_nanos: 130,
                kind: AgentEventKind::InputResolved {
                    request_id: Some("other".into()),
                },
            },
            t0 + 7 * S,
            false,
        );
        assert!(a.attention().is_some(), "mismatched request cannot clear");
        a.on_agent_event(
            AgentEvent {
                observed_at_unix_nanos: 140,
                kind: AgentEventKind::InputResolved {
                    request_id: Some("q1".into()),
                },
            },
            t0 + 8 * S,
            false,
        );
        assert!(a.attention().is_none());
        assert_eq!(a.turn, Turn::Idle);
        assert_eq!(a.working_since(), Some(t0 + 8 * S));
    }

    #[test]
    fn user_cancel_clears_attention_and_rejects_an_older_queued_packet() {
        let mut a = Activity::default();
        let t0 = base();
        a.on_agent_event(needs(NeedsInputKind::Input, None, 100), t0, true);
        assert!(a.attention().is_some());
        a.on_user_cancel(200);
        assert!(a.attention().is_none());
        a.on_agent_event(needs(NeedsInputKind::Approval, None, 150), t0 + S, false);
        assert!(
            a.attention().is_none(),
            "packet emitted before newer input stays cleared"
        );
        a.on_agent_event(
            needs(NeedsInputKind::Approval, None, 250),
            t0 + 2 * S,
            false,
        );
        assert_eq!(a.attention().unwrap().kind, NeedsInputKind::Approval);
        a.on_user_cancel(240);
        assert!(
            a.attention().is_some(),
            "an older input boundary cannot clear a newer prompt"
        );
        a.on_agent_event(
            AgentEvent {
                observed_at_unix_nanos: 240,
                kind: AgentEventKind::TurnStarted,
            },
            t0 + 3 * S,
            true,
        );
        assert!(
            a.attention().is_some(),
            "an older turn-start packet cannot clear a newer wait"
        );
        a.on_agent_event(
            AgentEvent {
                observed_at_unix_nanos: 260,
                kind: AgentEventKind::TurnStarted,
            },
            t0 + 4 * S,
            true,
        );
        assert!(a.attention().is_none());
        assert_eq!(a.working_since(), Some(t0 + 4 * S));
    }

    #[test]
    fn native_turn_start_is_visible_immediately_but_does_not_credit_idle_time() {
        let mut a = Activity::default();
        let t0 = base();
        a.on_agent_event(
            AgentEvent {
                observed_at_unix_nanos: 100,
                kind: AgentEventKind::TurnStarted,
            },
            t0,
            true,
        );

        assert_eq!(a.working_since(), Some(t0));
        assert_eq!(a.active, Duration::ZERO);
        assert_eq!(
            a.observe(t0 + 5 * S, 20 * S, Some(5 * S), true),
            Duration::ZERO,
            "silent in-flight wall time is not persisted as active output"
        );
        assert_eq!(a.working_since(), Some(t0));
    }

    #[test]
    fn native_start_accounts_fresh_output_without_backdating_silent_latency() {
        let mut a = Activity::default();
        let t0 = base();
        a.on_agent_event(
            AgentEvent {
                observed_at_unix_nanos: 100,
                kind: AgentEventKind::TurnStarted,
            },
            t0,
            true,
        );

        // Five seconds of model latency preceded this first fresh-output
        // sample. OBS_DELTA_MAX recognizes only the current output interval;
        // heuristic promotion must not seed/backdate all five seconds.
        let credited = a.observe(t0 + 5 * S, Duration::ZERO, Some(5 * S), true);
        assert_eq!(credited, S);
        assert_eq!(a.active, S);
        assert!(matches!(a.turn, Turn::Working { .. }));
        assert_eq!(a.working_since(), Some(t0));
    }

    #[test]
    fn provider_confirmation_keeps_the_earlier_local_resume_clock() {
        let mut a = Activity::default();
        let t0 = base();
        a.on_agent_event(needs(NeedsInputKind::Input, None, 100), t0, true);
        a.on_user_submit(120, t0 + S);
        assert_eq!(a.working_since(), Some(t0 + S));

        a.on_agent_event(
            AgentEvent {
                observed_at_unix_nanos: 140,
                kind: AgentEventKind::TurnStarted,
            },
            t0 + 2 * S,
            true,
        );
        assert_eq!(
            a.working_since(),
            Some(t0 + S),
            "the later hook corroborates rather than resets local timing"
        );
        assert!(a.native_working.as_ref().unwrap().provider_confirmed);
    }

    #[test]
    fn consecutive_native_turns_credit_each_output_interval_once() {
        let mut a = Activity::default();
        let t0 = base();
        a.on_agent_event(
            AgentEvent {
                observed_at_unix_nanos: 100,
                kind: AgentEventKind::TurnStarted,
            },
            t0,
            true,
        );
        assert_eq!(a.observe(t0 + S, Duration::ZERO, Some(S), true), S);
        a.on_agent_event(needs(NeedsInputKind::Input, None, 120), t0 + 2 * S, true);
        a.on_agent_event(
            AgentEvent {
                observed_at_unix_nanos: 140,
                kind: AgentEventKind::TurnStarted,
            },
            t0 + 3 * S,
            true,
        );
        assert_eq!(a.observe(t0 + 4 * S, Duration::ZERO, Some(S), true), S);
        assert_eq!(a.active, S, "current phase resets independently");
        assert_eq!(a.credited, 2 * S, "lifetime total never double-counts");
    }

    #[test]
    fn native_wait_freezes_visible_phase_and_stale_wait_cannot_follow_resume() {
        let mut a = Activity::default();
        let t0 = base();
        a.on_agent_event(
            AgentEvent {
                observed_at_unix_nanos: 100,
                kind: AgentEventKind::TurnStarted,
            },
            t0,
            false,
        );
        a.on_agent_event(
            needs(NeedsInputKind::Elicitation, Some("e1"), 120),
            t0 + 2 * S,
            false,
        );
        assert!(a.working_since().is_none());

        a.on_agent_event(
            AgentEvent {
                observed_at_unix_nanos: 140,
                kind: AgentEventKind::InputResolved {
                    request_id: Some("e1".into()),
                },
            },
            t0 + 4 * S,
            false,
        );
        assert_eq!(a.working_since(), Some(t0 + 4 * S));
        a.on_agent_event(
            needs(NeedsInputKind::Elicitation, Some("e1"), 130),
            t0 + 5 * S,
            false,
        );
        assert!(a.attention().is_none(), "older wait cannot re-latch");
        assert_eq!(a.working_since(), Some(t0 + 4 * S));
    }

    #[test]
    fn mouse_interaction_needs_sustained_output_before_resuming() {
        let mut a = Activity::default();
        let t0 = base();
        a.on_agent_event(needs(NeedsInputKind::Approval, None, 100), t0, true);
        a.on_user_interaction(200);
        // A click repaint inside echo grace is not enough.
        a.observe(t0 + S, Duration::ZERO, Some(S), true);
        assert!(a.attention().is_some());
        // Continued output after echo grace proves the child resumed.
        a.observe(t0 + 4 * S, Duration::ZERO, Some(4 * S), true);
        assert!(a.attention().is_none());
        assert_eq!(a.working_since(), Some(t0));
    }

    #[test]
    fn session_end_is_idle_not_a_resumed_turn() {
        let mut a = Activity::default();
        let t0 = base();
        a.on_agent_event(
            AgentEvent {
                observed_at_unix_nanos: 100,
                kind: AgentEventKind::TurnStarted,
            },
            t0,
            true,
        );
        a.on_agent_event(
            AgentEvent {
                observed_at_unix_nanos: 120,
                kind: AgentEventKind::SessionEnded,
            },
            t0 + S,
            true,
        );
        assert!(a.working_since().is_none());
        assert_eq!(a.turn, Turn::Idle);
    }

    #[test]
    fn native_turn_completion_restores_done_unread_semantics() {
        let mut a = Activity::default();
        let t0 = base();
        a.on_agent_event(
            AgentEvent {
                observed_at_unix_nanos: 100,
                kind: AgentEventKind::TurnStarted,
            },
            t0,
            false,
        );
        a.on_agent_event(
            AgentEvent {
                observed_at_unix_nanos: 120,
                kind: AgentEventKind::TurnCompleted,
            },
            t0 + 2 * S,
            false,
        );
        assert!(a.attention().is_none());
        assert!(a.working_since().is_none());
        assert!(matches!(
            a.turn,
            Turn::Done {
                finished,
                unread: true
            } if finished == t0 + 2 * S
        ));

        assert!(a.mark_viewed());
        assert!(!a.mark_viewed(), "viewing twice is not a state change");
        assert!(matches!(a.turn, Turn::Done { unread: false, .. }));
        assert!(a.working_since().is_none(), "viewing does not resume work");

        a.on_user_submit(140, t0 + 4 * S);
        assert_eq!(a.working_since(), Some(t0 + 4 * S));
    }

    #[test]
    fn stale_completion_cannot_overwrite_a_newer_approval() {
        let mut a = Activity::default();
        let t0 = base();
        a.on_agent_event(needs(NeedsInputKind::Approval, None, 200), t0, false);
        a.on_agent_event(
            AgentEvent {
                observed_at_unix_nanos: 150,
                kind: AgentEventKind::TurnCompleted,
            },
            t0 + S,
            false,
        );
        assert_eq!(a.attention().unwrap().kind, NeedsInputKind::Approval);
    }
}
