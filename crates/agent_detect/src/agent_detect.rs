//! Screen-scraping status detection for agent CLIs running inside terminals.
//!
//! Given the tail of a terminal's screen content plus its OSC title, classifies
//! what the agent process is doing: working, blocked on the user, or idle.
//! Rules match against narrow regions of the screen (the live prompt box, the
//! text after the last horizontal rule, ...) rather than the whole buffer so
//! that stale prompts in scrollback and spinner frames don't cause false
//! positives.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentKind {
    ClaudeCode,
    Codex,
}

impl AgentKind {
    /// Identify an agent from a foreground process command name (e.g. the
    /// value of `Terminal::foreground_process_command_name`).
    pub fn from_command_name(command: &str) -> Option<Self> {
        let name = command
            .rsplit(['/', '\\'])
            .next()
            .unwrap_or(command)
            .trim()
            .to_ascii_lowercase();
        let name = name.strip_suffix(".exe").unwrap_or(&name);
        match name {
            "claude" | "claude-code" => Some(Self::ClaudeCode),
            "codex" | "codex-cli" => Some(Self::Codex),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
        }
    }

    /// The executable used to launch this agent.
    pub fn executable(&self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude",
            Self::Codex => "codex",
        }
    }

    /// Command-line for resuming a previously captured session. The session
    /// reference is passed as argv data and must never be shell-interpolated.
    pub fn resume_argv(&self, session_ref: &str) -> Vec<String> {
        match self {
            Self::ClaudeCode => vec![
                "claude".into(),
                "--resume".into(),
                session_ref.into(),
            ],
            Self::Codex => vec!["codex".into(), "resume".into(), session_ref.into()],
        }
    }

    /// Command-line to use when the agent should be relaunched but no session
    /// was captured. Claude Code can resume the most recent conversation for
    /// the working directory; codex has no per-directory equivalent, so it
    /// starts fresh.
    pub fn relaunch_argv(&self) -> Vec<String> {
        match self {
            Self::ClaudeCode => vec!["claude".into(), "--continue".into()],
            Self::Codex => vec!["codex".into()],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AgentState {
    Idle,
    Working,
    Blocked,
    #[default]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Detection {
    pub state: AgentState,
    /// The live screen chrome itself shows this state (e.g. an empty prompt
    /// box for idle, a permission form for blocked) — stronger evidence than
    /// text found in scrollback.
    pub visible_idle: bool,
    pub visible_blocker: bool,
    pub visible_working: bool,
    /// The screen is showing a transcript viewer or similar overlay; its text
    /// must not be used to update state.
    pub skip_state_update: bool,
}

#[derive(Debug, Clone, Copy)]
pub struct DetectionInput<'a> {
    /// Tail of the terminal screen (the last ~40+ lines, newline-joined).
    pub screen: &'a str,
    /// The terminal's OSC 0/2 title as most recently set by the process.
    pub osc_title: &'a str,
}

/// Classify the agent's current state from the screen tail and OSC title.
/// Returns an idle detection when no rule matches: a known agent showing
/// nothing recognizable is assumed to be sitting at rest.
pub fn detect(agent: AgentKind, input: DetectionInput<'_>) -> Detection {
    let rules = rules_for(agent);
    let mut best: Option<&Rule> = None;
    for rule in rules {
        let region_text = slice_region(input, rule.region);
        let lower = region_text.to_lowercase();
        if !gate_matches(&rule.gate, region_text, &lower) {
            continue;
        }
        match best {
            Some(previous) if previous.priority >= rule.priority => {}
            _ => best = Some(rule),
        }
    }

    match best {
        Some(rule) => Detection {
            state: rule.state,
            visible_idle: rule.visible_idle && rule.state == AgentState::Idle,
            visible_blocker: rule.visible_blocker && rule.state == AgentState::Blocked,
            visible_working: rule.visible_working && rule.state == AgentState::Working,
            skip_state_update: rule.skip_state_update,
        },
        None => Detection {
            state: AgentState::Idle,
            visible_idle: false,
            visible_blocker: false,
            visible_working: false,
            skip_state_update: false,
        },
    }
}

/// Debounces Working -> Idle flapping: a momentary gap between spinner frames
/// must not flash the status to idle. The transition is held until it has been
/// re-observed [`Self::CONFIRMATIONS`] times or [`Self::HOLD_CAP`] has passed.
/// Blocked transitions and visibly-idle screens publish immediately.
#[derive(Debug, Default)]
pub struct StatusTracker {
    published: Option<AgentState>,
    pending_idle_started_at: Option<Instant>,
    pending_idle_confirmations: u8,
}

impl StatusTracker {
    const CONFIRMATIONS: u8 = 3;
    const HOLD_CAP: Duration = Duration::from_millis(700);

    pub fn published_state(&self) -> Option<AgentState> {
        self.published
    }

    /// A Working -> Idle transition is currently being held for confirmation.
    /// Callers should re-run detection shortly even if the terminal produces
    /// no further output, since a finished agent goes quiet.
    pub fn is_holding_idle(&self) -> bool {
        self.pending_idle_started_at.is_some()
    }

    /// Feed one detection; returns the newly published state when it changed.
    pub fn update(&mut self, detection: Detection, now: Instant) -> Option<AgentState> {
        if detection.skip_state_update {
            return None;
        }

        let next = detection.state;
        let holds = self.published == Some(AgentState::Working)
            && next == AgentState::Idle
            && !detection.visible_idle
            && !detection.visible_blocker;

        if holds {
            match self.pending_idle_started_at {
                None => {
                    self.pending_idle_started_at = Some(now);
                    self.pending_idle_confirmations = 0;
                    return None;
                }
                Some(started_at) => {
                    if now.duration_since(started_at) < Self::HOLD_CAP {
                        self.pending_idle_confirmations =
                            self.pending_idle_confirmations.saturating_add(1);
                        if self.pending_idle_confirmations < Self::CONFIRMATIONS {
                            return None;
                        }
                    }
                }
            }
        }
        self.pending_idle_started_at = None;
        self.pending_idle_confirmations = 0;

        if self.published == Some(next) {
            return None;
        }
        self.published = Some(next);
        Some(next)
    }

    /// The agent process is gone; the terminal is a plain shell again.
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

pub mod session_discovery {
    //! Locates the on-disk session an agent CLI is writing, so the terminal
    //! thread can be resumed after a restart. Purely correlational (newest
    //! session file for the working directory, modified since the agent
    //! started) — no agent configuration is touched.

    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::SystemTime;

    use super::AgentKind;

    /// Claude Code stores transcripts under
    /// `~/.claude/projects/<munged-cwd>/<session-id>.jsonl`, where the munged
    /// directory name replaces every non-alphanumeric character with `-`.
    pub fn claude_project_dir_name(cwd: &Path) -> String {
        cwd.to_string_lossy()
            .chars()
            .map(|character| {
                if character.is_ascii_alphanumeric() {
                    character
                } else {
                    '-'
                }
            })
            .collect()
    }

    /// `current_session` is the session previously captured for this
    /// terminal; when its file is still being written it is preferred over
    /// the globally newest one, so two agents sharing a working directory
    /// don't steal each other's sessions on every capture.
    pub fn find_session(
        agent: AgentKind,
        home_dir: &Path,
        cwd: &Path,
        since: SystemTime,
        current_session: Option<&str>,
    ) -> Option<String> {
        match agent {
            AgentKind::ClaudeCode => find_claude_session(home_dir, cwd, since, current_session),
            AgentKind::Codex => find_codex_session(home_dir, cwd, since, current_session),
        }
    }

    fn find_claude_session(
        home_dir: &Path,
        cwd: &Path,
        since: SystemTime,
        current_session: Option<&str>,
    ) -> Option<String> {
        let project_dir = home_dir
            .join(".claude")
            .join("projects")
            .join(claude_project_dir_name(cwd));

        if let Some(current) = current_session {
            let current_path = project_dir.join(format!("{current}.jsonl"));
            if fs::metadata(current_path)
                .and_then(|metadata| metadata.modified())
                .is_ok_and(|modified| modified >= since)
            {
                return Some(current.to_string());
            }
        }

        let mut newest: Option<(SystemTime, String)> = None;
        for entry in fs::read_dir(project_dir).ok()?.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|extension| extension != "jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else {
                continue;
            };
            if modified < since {
                continue;
            }
            if newest
                .as_ref()
                .is_none_or(|(newest_time, _)| modified > *newest_time)
            {
                newest = Some((modified, stem.to_string()));
            }
        }
        newest.map(|(_, session_id)| session_id)
    }

    /// Codex stores rollouts under
    /// `~/.codex/sessions/YYYY/MM/DD/rollout-<timestamp>-<session-id>.jsonl`;
    /// the first line records the session's working directory.
    fn find_codex_session(
        home_dir: &Path,
        cwd: &Path,
        since: SystemTime,
        current_session: Option<&str>,
    ) -> Option<String> {
        let sessions_dir = home_dir.join(".codex").join("sessions");
        let mut candidates: Vec<(SystemTime, PathBuf)> = Vec::new();
        collect_recent_files(&sessions_dir, since, 3, &mut candidates);
        candidates.sort_by(|(left, _), (right, _)| right.cmp(left));

        if let Some(current) = current_session
            && candidates.iter().any(|(_, path)| {
                codex_session_id_from_filename(path).as_deref() == Some(current)
            })
        {
            return Some(current.to_string());
        }

        // The first line is session metadata JSON; matching the exact quoted
        // value avoids capturing a sibling directory whose path merely starts
        // with this one (/repo/app vs /repo/app2).
        let quoted_cwd = serde_json_style_quoted(cwd.to_string_lossy().as_ref());
        for (_, path) in candidates.into_iter().take(20) {
            let Ok(contents) = fs::File::open(&path).map(std::io::BufReader::new) else {
                continue;
            };
            use std::io::BufRead as _;
            let Some(Ok(first_line)) = contents.lines().next() else {
                continue;
            };
            if !first_line.contains(&quoted_cwd) {
                continue;
            }
            if let Some(session_id) = codex_session_id_from_filename(&path) {
                return Some(session_id);
            }
        }
        None
    }

    /// The cwd as it appears as a JSON string value, including the closing
    /// quote so prefix paths don't match.
    fn serde_json_style_quoted(value: &str) -> String {
        let mut quoted = String::with_capacity(value.len() + 2);
        quoted.push('"');
        for character in value.chars() {
            match character {
                '"' => quoted.push_str("\\\""),
                '\\' => quoted.push_str("\\\\"),
                _ => quoted.push(character),
            }
        }
        quoted.push('"');
        quoted
    }

    fn collect_recent_files(
        dir: &Path,
        since: SystemTime,
        depth: usize,
        candidates: &mut Vec<(SystemTime, PathBuf)>,
    ) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if depth > 0 {
                    collect_recent_files(&path, since, depth - 1, candidates);
                }
                continue;
            }
            if path.extension().is_none_or(|extension| extension != "jsonl") {
                continue;
            }
            let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified()) else {
                continue;
            };
            if modified >= since {
                candidates.push((modified, path));
            }
        }
    }

    fn codex_session_id_from_filename(path: &Path) -> Option<String> {
        let stem = path.file_stem()?.to_str()?;
        // rollout-2026-07-21T22-15-03-<uuid>: the uuid is the last 36 chars.
        // Byte indexing would panic mid-codepoint on multibyte filenames, so
        // slice only at a verified char boundary.
        let split = stem.len().checked_sub(36)?;
        if !stem.is_char_boundary(split) {
            return None;
        }
        let candidate = &stem[split..];
        let is_uuid = candidate.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        });
        is_uuid.then(|| candidate.to_string())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::Write as _;
        use std::time::Duration;

        struct TempDir(PathBuf);

        impl TempDir {
            fn new(name: &str) -> Self {
                let path = std::env::temp_dir().join(format!(
                    "agent_detect_test_{}_{}",
                    name,
                    std::process::id()
                ));
                let _ = fs::remove_dir_all(&path);
                fs::create_dir_all(&path).expect("create temp dir");
                Self(path)
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = fs::remove_dir_all(&self.0);
            }
        }

        #[test]
        fn munges_cwd_like_claude_code() {
            assert_eq!(
                claude_project_dir_name(Path::new("/Users/foo/code/pay.kit_v2")),
                "-Users-foo-code-pay-kit-v2"
            );
        }

        #[test]
        fn finds_newest_claude_transcript_modified_since_launch() {
            let home = TempDir::new("claude");
            let cwd = Path::new("/repo/app");
            let project_dir = home
                .0
                .join(".claude")
                .join("projects")
                .join(claude_project_dir_name(cwd));
            fs::create_dir_all(&project_dir).expect("create project dir");

            let stale = project_dir.join("00000000-0000-0000-0000-000000000000.jsonl");
            fs::write(&stale, "{}").expect("write stale transcript");
            let since = SystemTime::now() + Duration::from_secs(1);

            assert_eq!(
                find_session(AgentKind::ClaudeCode, &home.0, cwd, since, None),
                None,
                "a transcript older than the launch must not be picked up"
            );

            let live = project_dir.join("11111111-2222-3333-4444-555555555555.jsonl");
            fs::write(&live, "{}").expect("write live transcript");
            let since = SystemTime::now() - Duration::from_secs(60);
            assert_eq!(
                find_session(AgentKind::ClaudeCode, &home.0, cwd, since, None).as_deref(),
                Some("11111111-2222-3333-4444-555555555555")
            );
        }

        #[test]
        fn prefers_current_claude_session_still_being_written() {
            let home = TempDir::new("claude_sticky");
            let cwd = Path::new("/repo/app");
            let project_dir = home
                .0
                .join(".claude")
                .join("projects")
                .join(claude_project_dir_name(cwd));
            fs::create_dir_all(&project_dir).expect("create project dir");

            let mine = "11111111-2222-3333-4444-555555555555";
            let other = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
            fs::write(project_dir.join(format!("{mine}.jsonl")), "{}").expect("write mine");
            fs::write(project_dir.join(format!("{other}.jsonl")), "{}").expect("write other");

            let since = SystemTime::now() - Duration::from_secs(60);
            assert_eq!(
                find_session(AgentKind::ClaudeCode, &home.0, cwd, since, Some(mine)).as_deref(),
                Some(mine),
                "an actively-written current session must not be replaced by a newer sibling"
            );
        }

        #[test]
        fn codex_cwd_match_requires_exact_quoted_value() {
            let home = TempDir::new("codex_prefix");
            let cwd = Path::new("/repo/app");
            let day_dir = home
                .0
                .join(".codex")
                .join("sessions")
                .join("2026")
                .join("07")
                .join("21");
            fs::create_dir_all(&day_dir).expect("create sessions dir");

            let sibling = day_dir
                .join("rollout-2026-07-21T12-00-00-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl");
            let mut file = fs::File::create(&sibling).expect("create sibling rollout");
            writeln!(file, r#"{{"type":"session_meta","cwd":"/repo/app2"}}"#).expect("write");

            let since = SystemTime::now() - Duration::from_secs(60);
            assert_eq!(
                find_session(AgentKind::Codex, &home.0, cwd, since, None),
                None,
                "/repo/app must not match a session recorded for /repo/app2"
            );
        }

        #[test]
        fn codex_session_id_ignores_multibyte_filenames_without_panicking() {
            assert_eq!(
                codex_session_id_from_filename(Path::new("rollout-ünïcödé-😀😀😀😀😀😀😀.jsonl")),
                None
            );
        }

        #[test]
        fn finds_codex_rollout_matching_cwd() {
            let home = TempDir::new("codex");
            let cwd = Path::new("/repo/app");
            let day_dir = home
                .0
                .join(".codex")
                .join("sessions")
                .join("2026")
                .join("07")
                .join("21");
            fs::create_dir_all(&day_dir).expect("create sessions dir");

            let other = day_dir
                .join("rollout-2026-07-21T10-00-00-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee.jsonl");
            let mut file = fs::File::create(&other).expect("create other rollout");
            writeln!(file, r#"{{"type":"session_meta","cwd":"/elsewhere"}}"#).expect("write");

            let matching = day_dir
                .join("rollout-2026-07-21T11-00-00-12345678-9abc-def0-1234-56789abcdef0.jsonl");
            let mut file = fs::File::create(&matching).expect("create matching rollout");
            writeln!(file, r#"{{"type":"session_meta","cwd":"/repo/app"}}"#).expect("write");

            let since = SystemTime::now() - Duration::from_secs(60);
            assert_eq!(
                find_session(AgentKind::Codex, &home.0, cwd, since, None).as_deref(),
                Some("12345678-9abc-def0-1234-56789abcdef0")
            );
        }
    }
}

struct Rule {
    state: AgentState,
    priority: i32,
    region: Region,
    visible_idle: bool,
    visible_blocker: bool,
    visible_working: bool,
    skip_state_update: bool,
    gate: Gate,
}

impl Rule {
    fn new(state: AgentState, priority: i32, region: Region) -> Self {
        Self {
            state,
            priority,
            region,
            visible_idle: false,
            visible_blocker: false,
            visible_working: false,
            skip_state_update: false,
            gate: Gate::default(),
        }
    }

    fn visible(mut self) -> Self {
        match self.state {
            AgentState::Idle => self.visible_idle = true,
            AgentState::Blocked => self.visible_blocker = true,
            AgentState::Working => self.visible_working = true,
            AgentState::Unknown => {}
        }
        self
    }

    fn skip_update(mut self) -> Self {
        self.skip_state_update = true;
        self
    }

    fn gate(mut self, gate: Gate) -> Self {
        self.gate = gate;
        self
    }
}

#[derive(Default)]
struct Gate {
    contains: Vec<&'static str>,
    regex: Vec<Regex>,
    line_regex: Vec<Regex>,
    all: Vec<Gate>,
    any: Vec<Gate>,
    not: Vec<Gate>,
}

impl Gate {
    fn contains(needles: &[&'static str]) -> Self {
        Self {
            contains: needles.to_vec(),
            ..Default::default()
        }
    }

    fn regex(patterns: &[&str]) -> Self {
        Self {
            regex: compile(patterns),
            ..Default::default()
        }
    }

    fn line_regex(patterns: &[&str]) -> Self {
        Self {
            line_regex: compile(patterns),
            ..Default::default()
        }
    }

    fn with_any(mut self, any: Vec<Gate>) -> Self {
        self.any = any;
        self
    }

    fn with_all(mut self, all: Vec<Gate>) -> Self {
        self.all = all;
        self
    }

    fn with_not(mut self, not: Vec<Gate>) -> Self {
        self.not = not;
        self
    }
}

fn compile(patterns: &[&str]) -> Vec<Regex> {
    patterns
        .iter()
        .map(|pattern| {
            #[allow(clippy::unwrap_used)]
            Regex::new(pattern).unwrap()
        })
        .collect()
}

/// Every matcher on a gate must hold: all `contains` substrings present
/// (case-insensitive), all `regex` matching the region, each `line_regex`
/// matching at least one line, all `all` sub-gates matching, at least one
/// `any` sub-gate matching (when non-empty), and no `not` sub-gate matching.
fn gate_matches(gate: &Gate, text: &str, lower_text: &str) -> bool {
    gate.contains
        .iter()
        .all(|needle| lower_text.contains(needle))
        && gate.regex.iter().all(|regex| regex.is_match(text))
        && gate
            .line_regex
            .iter()
            .all(|regex| text.lines().any(|line| regex.is_match(line)))
        && gate
            .all
            .iter()
            .all(|nested| gate_matches(nested, text, lower_text))
        && (gate.any.is_empty()
            || gate
                .any
                .iter()
                .any(|nested| gate_matches(nested, text, lower_text)))
        && !gate
            .not
            .iter()
            .any(|nested| gate_matches(nested, text, lower_text))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Region {
    OscTitle,
    WholeRecent,
    BottomNonEmptyLines(usize),
    /// Everything after the last `─...` horizontal-rule line — the live
    /// portion of a boxed TUI below its last divider.
    AfterLastHorizontalRule,
    /// The body of the input box: the lines between the last two
    /// horizontal-rule borders.
    PromptBoxBody,
    /// Everything after the last codex `›` prompt line.
    AfterLastPromptMarker,
}

fn slice_region<'a>(input: DetectionInput<'a>, region: Region) -> &'a str {
    let content = input.screen;
    match region {
        Region::OscTitle => input.osc_title,
        Region::WholeRecent => content,
        Region::BottomNonEmptyLines(count) => bottom_non_empty_lines(content, count),
        Region::AfterLastHorizontalRule => after_last_horizontal_rule(content),
        Region::PromptBoxBody => prompt_box_body(content).unwrap_or(""),
        Region::AfterLastPromptMarker => after_last_prompt_marker(content),
    }
}

fn line_start_offset(content: &str, lines: &[&str], index: usize) -> usize {
    lines[..index.min(lines.len())]
        .iter()
        .map(|line| line.len() + 1)
        .sum::<usize>()
        .min(content.len())
}

fn bottom_non_empty_lines(content: &str, count: usize) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(start_index) = lines
        .iter()
        .enumerate()
        .rev()
        .filter(|(_, line)| !line.trim().is_empty())
        .take(count)
        .last()
        .map(|(index, _)| index)
    else {
        return "";
    };
    &content[line_start_offset(content, &lines, start_index)..]
}

fn after_last_horizontal_rule(content: &str) -> &str {
    let mut last_rule_end = 0usize;
    let mut offset = 0usize;
    for line in content.lines() {
        let next_offset = offset + line.len() + 1;
        if is_horizontal_rule(line) {
            last_rule_end = next_offset.min(content.len());
        }
        offset = next_offset;
    }
    &content[last_rule_end..]
}

fn prompt_box_body(content: &str) -> Option<&str> {
    let lines: Vec<&str> = content.lines().collect();
    let top = prompt_box_top_border_index(&lines)?;
    let start = line_start_offset(content, &lines, top + 1);
    let end_index = lines[top + 1..]
        .iter()
        .position(|line| is_horizontal_rule(line))
        .map(|relative| top + 1 + relative)
        .unwrap_or(lines.len());
    let end = line_start_offset(content, &lines, end_index);
    Some(&content[start..end.max(start)])
}

fn prompt_box_top_border_index(lines: &[&str]) -> Option<usize> {
    let mut border_count = 0;
    for index in (0..lines.len()).rev() {
        if is_horizontal_rule(lines[index]) {
            border_count += 1;
            if border_count == 2 {
                return Some(index);
            }
        }
    }
    None
}

fn is_horizontal_rule(line: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    let rule_chars = trimmed.chars().take_while(|&ch| ch == '─').count();
    if rule_chars == 0 {
        return false;
    }
    let rule_bytes = trimmed
        .char_indices()
        .nth(rule_chars)
        .map(|(index, _)| index)
        .unwrap_or(trimmed.len());
    let suffix = trimmed[rule_bytes..].trim_start();
    suffix.is_empty() || rule_chars >= 3
}

fn after_last_prompt_marker(content: &str) -> &str {
    let lines: Vec<&str> = content.lines().collect();
    let Some(index) = lines
        .iter()
        .rposition(|line| *line == "›" || line.starts_with("› "))
    else {
        return content;
    };
    &content[line_start_offset(content, &lines, index + 1)..]
}

fn rules_for(agent: AgentKind) -> &'static [Rule] {
    match agent {
        AgentKind::ClaudeCode => claude_rules(),
        AgentKind::Codex => codex_rules(),
    }
}

fn claude_rules() -> &'static [Rule] {
    static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
        vec![
            // A Braille spinner glyph leading the OSC title means a turn is
            // in flight.
            Rule::new(AgentState::Working, 1100, Region::OscTitle)
                .visible()
                .gate(Gate::regex(&[r"^[\x{2800}-\x{28FF}] "])),
            // Transcript viewer overlay: scrollback text, never a state
            // signal.
            Rule::new(AgentState::Unknown, 1000, Region::BottomNonEmptyLines(3))
                .skip_update()
                .gate(
                    Gate::contains(&["showing detailed transcript"]).with_any(vec![
                        Gate::contains(&["ctrl+o", "to toggle"]),
                        Gate::contains(&["ctrl+e", "show all"]),
                        Gate::contains(&["ctrl+e", "collapse"]),
                        Gate::contains(&["↑↓ scroll"]),
                        Gate::contains(&["? for shortcuts"]),
                    ]),
                ),
            // A live selection form (tool confirmation, elicitation, ...)
            // below the last divider.
            Rule::new(AgentState::Blocked, 980, Region::AfterLastHorizontalRule)
                .visible()
                .gate(
                    Gate::contains(&["enter to select", "esc to cancel"]).with_any(vec![
                        Gate::contains(&["tab/arrow keys to navigate"]),
                        Gate::contains(&["arrow keys to navigate"]),
                        Gate::contains(&["arrows to navigate"]),
                        Gate::contains(&["↑/↓ to navigate"]),
                        Gate::contains(&["↑↓ to navigate"]),
                    ]),
                ),
            // An empty input box (bare `❯` between borders) is a resting
            // prompt — unless the same box carries selection-form hints.
            Rule::new(AgentState::Idle, 950, Region::PromptBoxBody)
                .visible()
                .gate(Gate::line_regex(&[r"^\s*❯"]).with_not(vec![
                    Gate::contains(&["enter to select"]),
                    Gate::contains(&["esc to cancel"]),
                    Gate::contains(&["tab/arrow keys"]),
                    Gate::contains(&["arrow keys to navigate"]),
                    Gate::contains(&["↑/↓ to navigate"]),
                ])),
            // The model picker is browsing, not a permission request.
            Rule::new(AgentState::Unknown, 900, Region::WholeRecent)
                .skip_update()
                .gate(
                    Gate::contains(&["select model", "enter to set as default", "esc to cancel"])
                        .with_not(vec![
                            Gate::contains(&["do you want to proceed?"]),
                            Gate::contains(&["enter to select"]),
                        ]),
                ),
            // Startup folder-trust dialog blocks everything until answered.
            Rule::new(AgentState::Blocked, 860, Region::WholeRecent)
                .visible()
                .gate(
                    Gate::contains(&["do you trust the files in this folder?"]).with_all(vec![
                        Gate::default().with_any(vec![
                            Gate::line_regex(&[r"(?i)^\s*❯?\s*1\.\s*yes"]),
                            Gate::contains(&["enter to confirm"]),
                        ]),
                    ]),
                ),
            // Bash permission prompt with a numbered yes/no menu.
            Rule::new(AgentState::Blocked, 850, Region::WholeRecent)
                .visible()
                .gate(
                    Gate::contains(&["do you want to proceed?"])
                        .with_any(vec![
                            Gate::contains(&["bash command"]),
                            Gate::contains(&["bash("]),
                            Gate::contains(&["contains expansion"]),
                            Gate::contains(&["tab to amend"]),
                            Gate::contains(&["ctrl+e to explain"]),
                        ])
                        .with_all(vec![Gate::default().with_any(vec![
                            Gate::line_regex(&[r"(?i)^\s*❯?\s*yes\b"]),
                            Gate::line_regex(&[r"(?i)^\s*1\.\s*yes\b"]),
                            Gate::line_regex(&[r"(?i)^\s*2\.\s*no\b"]),
                        ])]),
                ),
            // Non-bash permission prompt below the last divider.
            Rule::new(AgentState::Blocked, 840, Region::AfterLastHorizontalRule)
                .visible()
                .gate(
                    Gate::contains(&["do you want to proceed?", "esc to cancel"]).with_all(vec![
                        Gate::default().with_any(vec![
                            Gate::line_regex(&[r"(?i)^\s*❯?\s*1\.\s*yes\b"]),
                            Gate::line_regex(&[r"(?i)^\s*2\.\s*yes\b"]),
                            Gate::line_regex(&[r"(?i)^\s*2\.\s*no\b"]),
                            Gate::line_regex(&[r"(?i)^\s*3\.\s*no\b"]),
                        ]),
                    ]),
                ),
            // Older / uncommon confirmation phrasings anywhere on screen, as
            // long as no empty prompt line proves the agent already moved on.
            Rule::new(AgentState::Blocked, 300, Region::WholeRecent).gate(
                Gate::default()
                    .with_any(vec![
                        Gate::contains(&["do you want to"]).with_any(vec![
                            Gate::contains(&["yes"]),
                            Gate::contains(&["❯"]),
                        ]),
                        Gate::contains(&["would you like to"]).with_any(vec![
                            Gate::contains(&["yes"]),
                            Gate::contains(&["❯"]),
                        ]),
                        Gate::contains(&["waiting for permission"]),
                        Gate::contains(&["do you want to allow this connection?"]),
                        Gate::contains(&["tab to amend"]),
                        Gate::contains(&["ctrl+e to explain"]),
                        Gate::contains(&["do you want to proceed?", "esc to cancel"]),
                    ])
                    .with_not(vec![Gate::regex(&[r"(?m)^\s*❯\s*$"])]),
            ),
            // Screen fallback for terminals whose OSC title isn't forwarded:
            // the live status line during generation offers esc to interrupt.
            Rule::new(AgentState::Working, 500, Region::BottomNonEmptyLines(6))
                .visible()
                .gate(Gate::contains(&["esc to interrupt"])),
            // ✳ leading the OSC title is the resting sparkle.
            Rule::new(AgentState::Idle, 250, Region::OscTitle)
                .visible()
                .gate(Gate::regex(&[r"^\x{2733} "])),
        ]
    });
    &RULES
}

fn codex_rules() -> &'static [Rule] {
    static RULES: LazyLock<Vec<Rule>> = LazyLock::new(|| {
        vec![
            Rule::new(AgentState::Blocked, 1100, Region::OscTitle)
                .visible()
                .gate(Gate::contains(&["action required"])),
            Rule::new(AgentState::Working, 1050, Region::OscTitle)
                .visible()
                .gate(Gate::regex(&["(?:^| )[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏](?: |$)"])),
            Rule::new(AgentState::Unknown, 1000, Region::AfterLastPromptMarker)
                .skip_update()
                .gate(
                    Gate::contains(&[
                        "↑/↓ to scroll",
                        "pgup/pgdn to",
                        "home/end to jump",
                        "q to quit",
                    ])
                    .with_any(vec![
                        Gate::contains(&["esc to edit prev"]),
                        Gate::contains(&["esc/← to edit prev"]),
                    ]),
                ),
            Rule::new(AgentState::Blocked, 900, Region::AfterLastPromptMarker)
                .visible()
                .gate(Gate::default().with_any(vec![
                    Gate::contains(&["press enter to confirm or esc to cancel"]),
                    Gate::contains(&["enter to submit answer"]),
                    Gate::contains(&["enter to submit all"]),
                    Gate::contains(&["allow command?"]),
                ])),
            Rule::new(AgentState::Blocked, 600, Region::WholeRecent).gate(
                Gate::default().with_any(vec![
                    Gate::contains(&["[y/n]"]),
                    Gate::contains(&["yes (y)"]),
                    Gate::contains(&["do you want to"]).with_any(vec![
                        Gate::contains(&["yes"]),
                        Gate::contains(&["❯"]),
                    ]),
                    Gate::contains(&["would you like to"]).with_any(vec![
                        Gate::contains(&["yes"]),
                        Gate::contains(&["❯"]),
                    ]),
                ]),
            ),
            Rule::new(AgentState::Working, 500, Region::BottomNonEmptyLines(3))
                .visible()
                .gate(
                    Gate::line_regex(&[
                        r"^[•◦]\s+Working \([^)]*esc to interrupt\)(?: · .*)?$",
                    ])
                    .with_not(vec![Gate::contains(&["■ conversation interrupted"])]),
                ),
            // Any static, non-spinner title means the last turn finished.
            Rule::new(AgentState::Idle, 100, Region::OscTitle)
                .visible()
                .gate(Gate::regex(&[r"\S"]).with_not(vec![
                    Gate::regex(&["(?:^| )[⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏](?: |$)"]),
                    Gate::contains(&["action required"]),
                ])),
        ]
    });
    &RULES
}

#[cfg(test)]
mod tests {
    use super::*;

    fn detect_screen(agent: AgentKind, screen: &str) -> Detection {
        detect(
            agent,
            DetectionInput {
                screen,
                osc_title: "",
            },
        )
    }

    fn detect_title(agent: AgentKind, osc_title: &str) -> Detection {
        detect(
            agent,
            DetectionInput {
                screen: "",
                osc_title,
            },
        )
    }

    #[test]
    fn identifies_agents_from_command_names() {
        assert_eq!(
            AgentKind::from_command_name("claude"),
            Some(AgentKind::ClaudeCode)
        );
        assert_eq!(
            AgentKind::from_command_name("/usr/local/bin/claude"),
            Some(AgentKind::ClaudeCode)
        );
        assert_eq!(AgentKind::from_command_name("codex"), Some(AgentKind::Codex));
        assert_eq!(AgentKind::from_command_name("CODEX.exe"), Some(AgentKind::Codex));
        assert_eq!(AgentKind::from_command_name("zsh"), None);
        assert_eq!(AgentKind::from_command_name("node"), None);
    }

    #[test]
    fn claude_spinner_title_is_working() {
        let detection = detect_title(AgentKind::ClaudeCode, "⠧ Reticulating…");
        assert_eq!(detection.state, AgentState::Working);
        assert!(detection.visible_working);
    }

    #[test]
    fn claude_sparkle_title_is_idle() {
        let detection = detect_title(AgentKind::ClaudeCode, "✳ claude");
        assert_eq!(detection.state, AgentState::Idle);
        assert!(detection.visible_idle);
    }

    #[test]
    fn claude_empty_prompt_box_is_idle() {
        let screen = "\
Some earlier output
──────────────────────────────
 ❯
──────────────────────────────
  ? for shortcuts";
        let detection = detect_screen(AgentKind::ClaudeCode, screen);
        assert_eq!(detection.state, AgentState::Idle);
        assert!(detection.visible_idle);
    }

    #[test]
    fn claude_selection_form_is_blocked() {
        let screen = "\
──────────────────────────────
 Do you want to make this edit?
 ❯ 1. Yes
   2. No
 Enter to select · Esc to cancel · Tab/arrow keys to navigate";
        let detection = detect_screen(AgentKind::ClaudeCode, screen);
        assert_eq!(detection.state, AgentState::Blocked);
        assert!(detection.visible_blocker);
    }

    #[test]
    fn claude_bash_permission_prompt_is_blocked() {
        let screen = "\
 Bash command
   cargo build
 Do you want to proceed?
 ❯ 1. Yes
   2. No, and tell Claude what to do differently";
        let detection = detect_screen(AgentKind::ClaudeCode, screen);
        assert_eq!(detection.state, AgentState::Blocked);
    }

    #[test]
    fn claude_prompt_box_with_selection_hints_is_not_idle() {
        let screen = "\
──────────────────────────────
 ❯ 1. Yes
 Enter to select · Esc to cancel · arrow keys to navigate
──────────────────────────────";
        let detection = detect_screen(AgentKind::ClaudeCode, screen);
        assert!(!detection.visible_idle);
    }

    #[test]
    fn claude_transcript_viewer_skips_state_update() {
        let screen = "\
old scrollback with Do you want to proceed?
Showing detailed transcript · ctrl+o to toggle";
        let detection = detect_screen(AgentKind::ClaudeCode, screen);
        assert!(detection.skip_state_update);
    }

    #[test]
    fn claude_folder_trust_dialog_is_blocked() {
        let screen = "\
 Do you trust the files in this folder?
 /Users/foo/repo
 ❯ 1. Yes, proceed
   2. No, exit";
        let detection = detect_screen(AgentKind::ClaudeCode, screen);
        assert_eq!(detection.state, AgentState::Blocked);
        assert!(detection.visible_blocker);
    }

    #[test]
    fn claude_esc_to_interrupt_line_is_working_without_osc_title() {
        let screen = "\
some output
✳ Shimmying… (2s · esc to interrupt)";
        let detection = detect_screen(AgentKind::ClaudeCode, screen);
        assert_eq!(detection.state, AgentState::Working);
        assert!(detection.visible_working);
    }

    #[test]
    fn claude_unrecognized_screen_falls_back_to_idle() {
        let detection = detect_screen(AgentKind::ClaudeCode, "plain shell output\n$ ");
        assert_eq!(detection.state, AgentState::Idle);
        assert!(!detection.visible_idle);
    }

    #[test]
    fn codex_action_required_title_is_blocked() {
        let detection = detect_title(AgentKind::Codex, "Action Required · codex");
        assert_eq!(detection.state, AgentState::Blocked);
        assert!(detection.visible_blocker);
    }

    #[test]
    fn codex_spinner_title_is_working() {
        let detection = detect_title(AgentKind::Codex, "⠹ codex");
        assert_eq!(detection.state, AgentState::Working);
    }

    #[test]
    fn codex_static_title_is_idle() {
        let detection = detect_title(AgentKind::Codex, "codex — done");
        assert_eq!(detection.state, AgentState::Idle);
        assert!(detection.visible_idle);
    }

    #[test]
    fn codex_working_status_line_is_working() {
        let screen = "\
› do the thing
• Working (12s · esc to interrupt)";
        let detection = detect_screen(AgentKind::Codex, screen);
        assert_eq!(detection.state, AgentState::Working);
        assert!(detection.visible_working);
    }

    #[test]
    fn codex_interrupted_working_line_is_not_working() {
        let screen = "\
■ Conversation interrupted
• Working (12s · esc to interrupt)";
        let detection = detect_screen(AgentKind::Codex, screen);
        assert_ne!(detection.state, AgentState::Working);
    }

    #[test]
    fn codex_allow_command_after_prompt_is_blocked() {
        let screen = "\
› run the migration
  Allow command?
  Press enter to confirm or esc to cancel";
        let detection = detect_screen(AgentKind::Codex, screen);
        assert_eq!(detection.state, AgentState::Blocked);
        assert!(detection.visible_blocker);
    }

    #[test]
    fn codex_stale_blocker_before_prompt_marker_is_ignored() {
        // The confirmation text sits above the newest `›` prompt line, so it
        // belongs to a finished exchange.
        let screen = "\
  Allow command?
  Press enter to confirm or esc to cancel
› ";
        let detection = detect_screen(AgentKind::Codex, screen);
        assert_ne!(detection.state, AgentState::Blocked);
    }

    #[test]
    fn tracker_holds_working_to_idle_until_confirmed() {
        let mut tracker = StatusTracker::default();
        let now = Instant::now();
        let working = Detection {
            state: AgentState::Working,
            visible_idle: false,
            visible_blocker: false,
            visible_working: true,
            skip_state_update: false,
        };
        let plain_idle = Detection {
            state: AgentState::Idle,
            visible_idle: false,
            visible_blocker: false,
            visible_working: false,
            skip_state_update: false,
        };

        assert_eq!(tracker.update(working, now), Some(AgentState::Working));
        assert_eq!(tracker.update(plain_idle, now), None);
        assert_eq!(
            tracker.update(plain_idle, now + Duration::from_millis(100)),
            None
        );
        assert_eq!(
            tracker.update(plain_idle, now + Duration::from_millis(200)),
            None
        );
        assert_eq!(
            tracker.update(plain_idle, now + Duration::from_millis(300)),
            Some(AgentState::Idle)
        );
    }

    #[test]
    fn tracker_publishes_visible_idle_immediately() {
        let mut tracker = StatusTracker::default();
        let now = Instant::now();
        let working = Detection {
            state: AgentState::Working,
            visible_idle: false,
            visible_blocker: false,
            visible_working: true,
            skip_state_update: false,
        };
        let visible_idle = Detection {
            state: AgentState::Idle,
            visible_idle: true,
            visible_blocker: false,
            visible_working: false,
            skip_state_update: false,
        };

        assert_eq!(tracker.update(working, now), Some(AgentState::Working));
        assert_eq!(tracker.update(visible_idle, now), Some(AgentState::Idle));
    }

    #[test]
    fn tracker_publishes_blocked_immediately() {
        let mut tracker = StatusTracker::default();
        let now = Instant::now();
        let working = Detection {
            state: AgentState::Working,
            visible_idle: false,
            visible_blocker: false,
            visible_working: true,
            skip_state_update: false,
        };
        let blocked = Detection {
            state: AgentState::Blocked,
            visible_idle: false,
            visible_blocker: true,
            visible_working: false,
            skip_state_update: false,
        };

        assert_eq!(tracker.update(working, now), Some(AgentState::Working));
        assert_eq!(tracker.update(blocked, now), Some(AgentState::Blocked));
    }

    #[test]
    fn tracker_ignores_skip_state_update() {
        let mut tracker = StatusTracker::default();
        let now = Instant::now();
        let working = Detection {
            state: AgentState::Working,
            visible_idle: false,
            visible_blocker: false,
            visible_working: true,
            skip_state_update: false,
        };
        let skip = Detection {
            state: AgentState::Unknown,
            visible_idle: false,
            visible_blocker: false,
            visible_working: false,
            skip_state_update: true,
        };

        assert_eq!(tracker.update(working, now), Some(AgentState::Working));
        assert_eq!(tracker.update(skip, now), None);
        assert_eq!(tracker.published_state(), Some(AgentState::Working));
    }

    #[test]
    fn tracker_cap_expires_the_idle_hold() {
        let mut tracker = StatusTracker::default();
        let now = Instant::now();
        let working = Detection {
            state: AgentState::Working,
            visible_idle: false,
            visible_blocker: false,
            visible_working: true,
            skip_state_update: false,
        };
        let plain_idle = Detection {
            state: AgentState::Idle,
            visible_idle: false,
            visible_blocker: false,
            visible_working: false,
            skip_state_update: false,
        };

        assert_eq!(tracker.update(working, now), Some(AgentState::Working));
        assert_eq!(tracker.update(plain_idle, now), None);
        assert_eq!(
            tracker.update(plain_idle, now + Duration::from_millis(800)),
            Some(AgentState::Idle)
        );
    }

    #[test]
    fn resume_argv_keeps_session_ref_as_data() {
        let argv = AgentKind::ClaudeCode.resume_argv("abc; rm -rf /");
        assert_eq!(argv, vec!["claude", "--resume", "abc; rm -rf /"]);
        let argv = AgentKind::Codex.resume_argv("0197-abc");
        assert_eq!(argv, vec!["codex", "resume", "0197-abc"]);
    }
}
