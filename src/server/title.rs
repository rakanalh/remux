//! Which `OSC 0` / `OSC 2` window title a pane is named by.
//!
//! A title belongs to the process group that set it and names the pane only
//! while that group is the pane's effective foreground. The shell's title and
//! the running job's title are kept apart, so a job never costs the shell its
//! title. A title reaches the name only after it has stayed unchanged for
//! [`SETTLE`], and a job becomes the effective foreground only after holding
//! the terminal for [`SETTLE`], so a short command never changes the name.
//!
//! Everything here is pure: the caller supplies the clock, the process groups
//! and whether a group is still alive, so the rules are unit-tested without a
//! PTY.

use std::time::{Duration, Instant};

/// How long a title must stay unchanged before it names the pane, and how long
/// a job must hold the foreground before it does.
///
/// A shell `preexec` hook that sets the title to the command and a `precmd`
/// hook that sets it back finish within milliseconds for a short command, so
/// any window far above that keeps them off every label. One second is also
/// short enough that a program which sets its title once, such as an agent
/// naming its task, is labelled before the user looks for it.
pub const SETTLE: Duration = Duration::from_millis(1000);

/// How soon before a job takes the foreground a title the shell set counts as
/// that job's `preexec` title.
///
/// `preexec` runs just before the shell starts the command, so its title
/// arrives milliseconds before the job does. A title the user set at the
/// prompt precedes the next command by at least the time it takes to type one,
/// and is kept.
const PREEXEC_GAP: Duration = Duration::from_millis(500);

/// `raw` as it is compared and shown: trimmed, and without a leading glyph.
///
/// Claude Code titles itself `✳ <task>`, and other programs animate a spinner
/// in that position. Stripped, a spinner frame is the same title as the frame
/// before it, so it neither restarts the settle window nor appears in a label.
/// Only a run of NON-ASCII symbols followed by whitespace is stripped, so an
/// ASCII prefix a program chose (`- vim`, `$ ls`, `~/src`) is kept.
pub fn normalise(raw: &str) -> String {
    let trimmed = raw.trim();
    let glyphs_end = trimmed
        .char_indices()
        .find(|(_, c)| c.is_ascii() || c.is_alphanumeric() || c.is_whitespace())
        .map(|(i, _)| i)
        .unwrap_or(trimmed.len());
    let rest = &trimmed[glyphs_end..];
    if glyphs_end > 0 && rest.starts_with(char::is_whitespace) {
        rest.trim_start().to_string()
    } else {
        trimmed.to_string()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Candidate {
    text: String,
    since: Instant,
    /// The process group that was in the foreground when the title was
    /// written.
    setter: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Job {
    pgid: i32,
    /// When a sample first showed this group in the foreground.
    since: Instant,
    /// Whether [`TitleTracker::foreground_due`] has reported the end of this
    /// job's [`SETTLE`] window.
    announced: bool,
}

/// What a foreground sample changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// The sample was older than one already applied, or changed nothing.
    None,
    /// A new job took the foreground.
    Job(i32),
    /// The shell took the foreground back from a job. `named` says whether
    /// the job had held the foreground long enough to name the pane.
    Shell { named: bool },
}

/// One pane's title state.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TitleTracker {
    candidate: Option<Candidate>,
    shell_title: Option<String>,
    /// The title a job set, with the job's process group.
    job_title: Option<(i32, String)>,
    /// The job in the foreground, when it is not the shell.
    job: Option<Job>,
    /// When the newest applied foreground sample was taken.
    last_sample: Option<Instant>,
}

impl TitleTracker {
    /// Apply a foreground sample `fg` taken at `sampled_at`.
    ///
    /// A sample older than one already applied is ignored. Samples come from
    /// more than one place (the PTY reader tags each read, the forwarding loop
    /// polls while the pane is quiet), and an old sample applied late would
    /// restart a job's window or bring back a job that has exited.
    ///
    /// When the shell takes the foreground back, a job title whose group no
    /// longer exists is dropped, because its process group id can be reused by
    /// the next job. `alive` is asked only then. A stopped job (`Ctrl-Z`) is
    /// alive and keeps its title for `fg`.
    pub fn note_foreground(
        &mut self,
        fg: Option<i32>,
        sampled_at: Instant,
        shell: i32,
        alive: impl FnOnce(i32) -> bool,
    ) -> Transition {
        if self.last_sample.is_some_and(|last| sampled_at < last) {
            // Still evidence of when the current job took the foreground: an
            // idle poll can be applied before an earlier read from the same
            // job, and the job's window runs from the earlier of the two.
            if let Some(job) = self.job.as_mut().filter(|j| Some(j.pgid) == fg) {
                job.since = job.since.min(sampled_at);
            }
            return Transition::None;
        }
        self.last_sample = Some(sampled_at);
        let fg = fg.unwrap_or(shell);
        if fg == shell {
            let Some(job) = self.job.take() else {
                return Transition::None;
            };
            // A `preexec` title named the command that has just finished, and
            // `precmd` is about to set the prompt's title again, so it is
            // dropped rather than shown at the prompt for a settle window.
            if self
                .candidate
                .as_ref()
                .is_some_and(|c| c.setter == shell && c.since + PREEXEC_GAP >= job.since)
            {
                self.candidate = None;
            }
            if let Some((owner, _)) = &self.job_title {
                if !alive(*owner) {
                    self.job_title = None;
                }
            }
            let named = sampled_at.saturating_duration_since(job.since) >= SETTLE;
            return Transition::Shell { named };
        }
        if self.job.as_ref().is_some_and(|j| j.pgid == fg) {
            return Transition::None;
        }
        self.job = Some(Job {
            pgid: fg,
            since: sampled_at,
            announced: false,
        });
        Transition::Job(fg)
    }

    /// The job in the foreground, whether or not it names the pane yet.
    pub fn job(&self) -> Option<i32> {
        self.job.as_ref().map(|j| j.pgid)
    }

    /// The process group the pane is named after at `now`: the foreground job
    /// once it has held the foreground for [`SETTLE`], else the shell.
    pub fn effective(&self, now: Instant, shell: i32) -> i32 {
        match (&self.job, self.job_named_at()) {
            (Some(j), Some(at)) if now >= at => j.pgid,
            _ => shell,
        }
    }

    /// When the current job starts to name the pane: [`SETTLE`] after it took
    /// the foreground. `None` while a title the job wrote within that time is
    /// still waiting to settle: the job then names the pane only once
    /// [`TitleTracker::settle`] adopts that title, so its process name never
    /// flashes up between the two.
    fn job_named_at(&self) -> Option<Instant> {
        let job = self.job.as_ref()?;
        let at = job.since + SETTLE;
        match &self.candidate {
            Some(c) if c.setter == job.pgid && c.since < at => None,
            _ => Some(at),
        }
    }

    /// Whether the current job's [`SETTLE`] window ended by `now` and has not
    /// been reported yet. Reported once, so that the caller refreshes the
    /// labels once.
    pub fn foreground_due(&mut self, now: Instant) -> bool {
        let due = self.job_named_at().is_some_and(|at| now >= at);
        match &mut self.job {
            Some(j) if !j.announced && due => {
                j.announced = true;
                true
            }
            _ => false,
        }
    }

    /// Record a normalised title written at `now` while `setter` was the
    /// foreground process group.
    pub fn observe(&mut self, text: String, now: Instant, setter: i32, shell: i32) {
        if self
            .candidate
            .as_ref()
            .is_some_and(|c| c.text == text && c.setter == setter)
        {
            return;
        }
        if self.title_of(setter, shell).unwrap_or("") == text {
            // A preexec title reset by precmd lands here, so the short command's
            // title is dropped before it can settle.
            self.candidate = None;
            return;
        }
        self.candidate = Some(Candidate {
            text,
            since: now,
            setter,
        });
    }

    /// Whether a candidate has waited out [`SETTLE`] by `now`.
    pub fn due(&self, now: Instant) -> bool {
        self.candidate
            .as_ref()
            .is_some_and(|c| now.saturating_duration_since(c.since) >= SETTLE)
    }

    /// Adopt or drop a due candidate. Returns whether an adopted title changed.
    ///
    /// A title the shell wrote is the shell's, whatever runs next. A title a
    /// job wrote is adopted only if that job is the effective foreground at
    /// `now`. Otherwise the job exited during the window, or never held the
    /// foreground long enough to be named, and the title is dropped.
    pub fn settle(&mut self, now: Instant, shell: i32) -> bool {
        if !self.due(now) {
            return false;
        }
        let Some(candidate) = self.candidate.take() else {
            return false;
        };
        if candidate.setter == shell && self.job.is_some() {
            // The shell's title names the pane only at the prompt, so it is
            // held until the job ends. See `note_foreground` for what happens
            // to it then.
            self.candidate = Some(candidate);
            return false;
        }
        let text = (!candidate.text.is_empty()).then_some(candidate.text);
        if candidate.setter == shell {
            let changed = self.shell_title != text;
            self.shell_title = text;
            return changed;
        }
        if candidate.setter != self.effective(now, shell) {
            return false;
        }
        let next = text.map(|t| (candidate.setter, t));
        let changed = self.job_title != next;
        self.job_title = next;
        changed
    }

    /// The adopted title that names the pane while `effective` is its
    /// effective foreground.
    pub fn current(&self, effective: i32, shell: i32) -> Option<&str> {
        self.title_of(effective, shell)
    }

    fn title_of(&self, group: i32, shell: i32) -> Option<&str> {
        if group == shell {
            return self.shell_title.as_deref();
        }
        self.job_title
            .as_ref()
            .filter(|(owner, _)| *owner == group)
            .map(|(_, t)| t.as_str())
    }

    #[cfg(test)]
    fn pending(&self) -> bool {
        self.candidate.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHELL: i32 = 100;
    const PROG: i32 = 200;

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    fn fg(t: &mut TitleTracker, group: i32, when: Instant) -> Transition {
        t.note_foreground(Some(group), when, SHELL, |_| true)
    }

    fn shown(t: &TitleTracker, now: Instant) -> Option<&str> {
        t.current(t.effective(now, SHELL), SHELL)
    }

    fn with_shell_title(text: &str, t0: Instant) -> TitleTracker {
        let mut t = TitleTracker::default();
        t.observe(text.into(), t0, SHELL, SHELL);
        assert!(t.settle(at(t0, 1000), SHELL));
        t
    }

    #[test]
    fn normalise_strips_a_non_ascii_glyph_prefix_only() {
        assert_eq!(normalise("✳ Claude Code"), "Claude Code");
        assert_eq!(normalise("⠂  Fix the bug "), "Fix the bug");
        assert_eq!(normalise("✳\u{fe0f} Task"), "Task");
        assert_eq!(normalise("● ready"), "ready");
        assert_eq!(normalise("~/src/remux"), "~/src/remux");
        assert_eq!(normalise("- vim"), "- vim");
        assert_eq!(normalise("$ ls"), "$ ls");
        assert_eq!(normalise("✳"), "✳");
        assert_eq!(normalise("   "), "");
    }

    #[test]
    fn spinner_frames_do_not_restart_the_window() {
        let t0 = Instant::now();
        let mut t = TitleTracker::default();
        fg(&mut t, PROG, t0);
        t.observe(normalise("⠂ Task"), t0, PROG, SHELL);
        t.observe(normalise("⠐ Task"), at(t0, 900), PROG, SHELL);
        assert!(t.settle(at(t0, 1000), SHELL));
        assert_eq!(shown(&t, at(t0, 1000)), Some("Task"));
    }

    #[test]
    fn a_title_is_not_adopted_before_the_window() {
        let t0 = Instant::now();
        let mut t = TitleTracker::default();
        t.observe("Hello".into(), t0, SHELL, SHELL);
        assert!(!t.settle(at(t0, 999), SHELL));
        assert_eq!(shown(&t, at(t0, 999)), None);
        assert!(t.settle(at(t0, 1000), SHELL));
        assert_eq!(shown(&t, at(t0, 1000)), Some("Hello"));
    }

    #[test]
    fn a_title_replaced_within_the_window_never_appears() {
        let t0 = Instant::now();
        let mut t = TitleTracker::default();
        t.observe("ls".into(), t0, SHELL, SHELL);
        t.observe("prompt".into(), at(t0, 5), SHELL, SHELL);
        assert!(!t.due(at(t0, 1000)));
        assert!(t.settle(at(t0, 1005), SHELL));
        assert_eq!(shown(&t, at(t0, 1005)), Some("prompt"));
    }

    #[test]
    fn precmd_restoring_the_shown_title_cancels_the_preexec_one() {
        let t0 = Instant::now();
        let mut t = with_shell_title("~/src", t0);
        t.observe("ls".into(), at(t0, 2000), SHELL, SHELL);
        t.observe("~/src".into(), at(t0, 2005), SHELL, SHELL);
        assert!(!t.pending());
        assert_eq!(shown(&t, at(t0, 5000)), Some("~/src"));
    }

    #[test]
    fn a_short_job_never_displaces_the_shell_title() {
        let t0 = Instant::now();
        let mut t = with_shell_title("~/src", t0);
        fg(&mut t, PROG, at(t0, 2000));
        assert_eq!(shown(&t, at(t0, 2999)), Some("~/src"));
        fg(&mut t, SHELL, at(t0, 2999));
        assert_eq!(shown(&t, at(t0, 3500)), Some("~/src"));
    }

    #[test]
    fn a_long_job_is_named_by_itself_and_the_shell_title_comes_straight_back() {
        let t0 = Instant::now();
        let mut t = with_shell_title("~/src", t0);
        assert_eq!(fg(&mut t, PROG, at(t0, 2000)), Transition::Job(PROG));
        assert!(!t.foreground_due(at(t0, 2500)));
        assert!(t.foreground_due(at(t0, 3000)));
        assert!(!t.foreground_due(at(t0, 3100)), "reported once");
        assert_eq!(t.effective(at(t0, 3000), SHELL), PROG);
        assert_eq!(shown(&t, at(t0, 3000)), None);
        assert_eq!(
            fg(&mut t, SHELL, at(t0, 4000)),
            Transition::Shell { named: true }
        );
        assert_eq!(shown(&t, at(t0, 4000)), Some("~/src"));
    }

    #[test]
    fn a_jobs_title_does_not_replace_the_shells() {
        let t0 = Instant::now();
        let mut t = with_shell_title("Home", t0);
        fg(&mut t, PROG, at(t0, 2000));
        t.observe("Job".into(), at(t0, 2000), PROG, SHELL);
        assert!(t.settle(at(t0, 3000), SHELL));
        assert_eq!(shown(&t, at(t0, 3000)), Some("Job"));
        t.note_foreground(Some(SHELL), at(t0, 4000), SHELL, |_| false);
        assert_eq!(shown(&t, at(t0, 4000)), Some("Home"));
    }

    #[test]
    fn a_shell_title_that_comes_due_during_a_short_job_stays_the_shells() {
        let t0 = Instant::now();
        let mut t = TitleTracker::default();
        t.observe("Due".into(), t0, SHELL, SHELL);
        fg(&mut t, PROG, at(t0, 995));
        assert!(!t.settle(at(t0, 1000), SHELL), "held while the job runs");
        fg(&mut t, SHELL, at(t0, 1005));
        assert!(
            t.settle(at(t0, 1005), SHELL),
            "adopted once the shell is back"
        );
        assert_eq!(shown(&t, at(t0, 1005)), Some("Due"));
    }

    #[test]
    fn a_shell_title_does_not_name_a_long_job() {
        let t0 = Instant::now();
        let mut t = TitleTracker::default();
        t.observe("top".into(), t0, SHELL, SHELL);
        fg(&mut t, PROG, at(t0, 1));
        assert!(!t.settle(at(t0, 1000), SHELL));
        assert_eq!(shown(&t, at(t0, 1001)), None);
        fg(&mut t, SHELL, at(t0, 3000));
        assert!(!t.settle(at(t0, 3000), SHELL));
        assert_eq!(
            shown(&t, at(t0, 3000)),
            None,
            "a preexec title is not shown at the prompt after its command"
        );
    }

    #[test]
    fn precmd_restoring_the_prompt_title_after_a_long_job_is_a_no_op() {
        let t0 = Instant::now();
        let mut t = with_shell_title("Home", t0);
        t.observe("sleep 2".into(), at(t0, 2000), SHELL, SHELL);
        fg(&mut t, PROG, at(t0, 2005));
        assert!(!t.settle(at(t0, 3000), SHELL), "held while the job runs");
        fg(&mut t, SHELL, at(t0, 4005));
        t.observe("Home".into(), at(t0, 4006), SHELL, SHELL);
        assert!(!t.pending());
        assert_eq!(shown(&t, at(t0, 4006)), Some("Home"));
    }

    #[test]
    fn a_job_that_titles_itself_early_is_never_shown_by_its_process_name() {
        let t0 = Instant::now();
        let mut t = with_shell_title("Home", t0);
        fg(&mut t, PROG, at(t0, 2000));
        t.observe("Job".into(), at(t0, 2010), PROG, SHELL);
        assert_eq!(t.effective(at(t0, 3005), SHELL), SHELL);
        assert!(!t.foreground_due(at(t0, 3005)));
        assert_eq!(
            t.effective(at(t0, 3015), SHELL),
            SHELL,
            "still the shell until the title is actually adopted"
        );
        assert!(t.settle(at(t0, 3015), SHELL));
        assert!(t.foreground_due(at(t0, 3015)));
        assert_eq!(shown(&t, at(t0, 3010)), Some("Job"));
    }

    #[test]
    fn a_title_whose_setter_exited_in_the_window_is_dropped() {
        let t0 = Instant::now();
        let mut t = TitleTracker::default();
        fg(&mut t, PROG, t0);
        t.observe("gone".into(), t0, PROG, SHELL);
        fg(&mut t, SHELL, at(t0, 5));
        assert!(!t.settle(at(t0, 1000), SHELL));
        assert!(!t.pending());
        assert_eq!(shown(&t, at(t0, 1000)), None);
    }

    #[test]
    fn a_dead_jobs_title_is_dropped_when_the_shell_is_back() {
        let t0 = Instant::now();
        let mut t = TitleTracker::default();
        fg(&mut t, PROG, t0);
        t.observe("Claude".into(), t0, PROG, SHELL);
        t.settle(at(t0, 1000), SHELL);
        t.note_foreground(Some(SHELL), at(t0, 2000), SHELL, |_| false);
        fg(&mut t, PROG, at(t0, 3000));
        assert_eq!(shown(&t, at(t0, 4000)), None, "a reused pgid is not named");
    }

    #[test]
    fn a_stopped_jobs_title_survives_for_fg() {
        let t0 = Instant::now();
        let mut t = TitleTracker::default();
        fg(&mut t, PROG, t0);
        t.observe("Claude".into(), t0, PROG, SHELL);
        t.settle(at(t0, 1000), SHELL);
        t.note_foreground(Some(SHELL), at(t0, 2000), SHELL, |_| true);
        fg(&mut t, PROG, at(t0, 3000));
        assert_eq!(shown(&t, at(t0, 4000)), Some("Claude"));
    }

    #[test]
    fn a_stale_sample_cannot_restart_the_window_or_revive_a_job() {
        let t0 = Instant::now();
        let mut t = TitleTracker::default();
        fg(&mut t, PROG, t0);
        assert_eq!(
            fg(&mut t, SHELL, at(t0, 1500)),
            Transition::Shell { named: true }
        );
        assert_eq!(fg(&mut t, PROG, at(t0, 1400)), Transition::None);
        assert_eq!(t.effective(at(t0, 3000), SHELL), SHELL);
    }

    #[test]
    fn an_earlier_sample_of_the_same_job_moves_its_window_back() {
        let t0 = Instant::now();
        let mut t = TitleTracker::default();
        fg(&mut t, PROG, at(t0, 5));
        assert_eq!(fg(&mut t, PROG, t0), Transition::None);
        t.observe("Job".into(), t0, PROG, SHELL);
        assert!(
            t.settle(at(t0, 1000), SHELL),
            "adopted at the title's own window"
        );
    }

    #[test]
    fn an_empty_title_clears_the_adopted_one_after_the_window() {
        let t0 = Instant::now();
        let mut t = TitleTracker::default();
        fg(&mut t, PROG, t0);
        t.observe("Claude Code".into(), t0, PROG, SHELL);
        t.settle(at(t0, 1000), SHELL);
        t.observe(String::new(), at(t0, 2000), PROG, SHELL);
        assert_eq!(shown(&t, at(t0, 2000)), Some("Claude Code"));
        assert!(t.settle(at(t0, 3000), SHELL));
        assert_eq!(shown(&t, at(t0, 3000)), None);
    }
}
