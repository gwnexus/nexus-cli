//! `nexus status --agents [--watch] [--json]`: the native Agent
//! Observability renderer (NEXUS-APP ADR-0120, dispatch c4f507b5).
//!
//! Reads the append-only `nexus.agent-observation.v1` events the Nexus
//! Claude Code hook writes to `<agentic_root>/claude/observer/events.jsonl`,
//! folds them per session and agent, and renders the fleet: main agent,
//! subagents as a tree, background processes, state, current tool, runtime,
//! model and repository/worktree. Local only: no network, no auth, so it
//! also serves `--plain` sessions from a second terminal. `--watch` follows
//! the file (incremental reads, rotation and truncation handled) and redraws
//! once per second.

use std::collections::BTreeMap;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use console::style;
use serde::{Deserialize, Serialize};

/// Event file relative to the agentic root.
pub const EVENTS_REL: &str = "claude/observer/events.jsonl";

/// Sessions whose agents all finished longer ago than this are hidden from
/// the text view (`--all` shows them).
const FINISHED_SESSION_TTL_MS: i64 = 30 * 60 * 1000;

const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// One event line. Every field is optional; a missing value is unknown.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    ts: Option<serde_json::Value>,
    event: Option<String>,
    session_id: Option<String>,
    agent_id: Option<String>,
    parent_agent_id: Option<String>,
    role: Option<String>,
    state: Option<String>,
    task: Option<String>,
    tool: Option<String>,
    model: Option<String>,
    repository: Option<String>,
    cwd: Option<String>,
    worktree: Option<String>,
    started_at: Option<serde_json::Value>,
    runtime_ms: Option<u64>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}

/// The folded state of one agent (the `--json` DTO).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentView {
    pub session_id: Option<String>,
    pub agent_id: Option<String>,
    pub parent_agent_id: Option<String>,
    /// `main` | `subagent` | `background` (derived from the parent when
    /// the events never said).
    pub role: String,
    /// `working` | `waiting` | `done` | `failed` | `unknown`.
    pub state: String,
    pub task: Option<String>,
    pub tool: Option<String>,
    pub model: Option<String>,
    pub repository: Option<String>,
    pub cwd: Option<String>,
    pub worktree: Option<String>,
    /// Epoch milliseconds.
    pub started_at: Option<i64>,
    /// Runtime as of `now` (see [`AgentView::runtime_at`]).
    pub runtime_ms: Option<u64>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub last_event: Option<String>,
    /// Epoch milliseconds of the last event.
    pub last_event_at: Option<i64>,
    pub events: u64,
    #[serde(skip)]
    explicit_role: Option<String>,
    /// `runtimeMs` as reported, and when.
    #[serde(skip)]
    reported_runtime: Option<(u64, Option<i64>)>,
}

impl AgentView {
    fn apply(&mut self, r: Record) {
        let ts = r.ts.as_ref().and_then(parse_ts);
        macro_rules! take {
            ($($f:ident),*) => { $( if r.$f.is_some() { self.$f = r.$f; } )* };
        }
        take!(
            session_id,
            agent_id,
            parent_agent_id,
            task,
            tool,
            model,
            repository,
            cwd,
            worktree,
            input_tokens,
            output_tokens
        );
        if let Some(role) = r.role {
            self.explicit_role = Some(role);
        }
        if let Some(state) = r.state {
            self.state = state;
        }
        if let Some(started) = r.started_at.as_ref().and_then(parse_ts) {
            self.started_at = Some(started);
        }
        if let Some(ms) = r.runtime_ms {
            self.reported_runtime = Some((ms, ts));
        }
        if r.event.is_some() {
            self.last_event = r.event;
        }
        if let Some(ts) = ts {
            self.last_event_at = Some(self.last_event_at.map_or(ts, |t| t.max(ts)));
        }
        self.events += 1;
        self.role = self.explicit_role.clone().unwrap_or_else(|| {
            if self.parent_agent_id.is_some() {
                "subagent".into()
            } else {
                "main".into()
            }
        });
    }

    fn is_working(&self) -> bool {
        self.state == "working"
    }

    fn is_finished(&self) -> bool {
        matches!(self.state.as_str(), "done" | "failed")
    }

    /// Runtime at `now`: a working agent counts up from `startedAt` (or from
    /// its last reported runtime); a finished one shows the reported runtime,
    /// else start to last event.
    pub fn runtime_at(&self, now: i64) -> Option<u64> {
        let since = |from: i64, to: i64| u64::try_from(to - from).ok();
        if self.is_working() {
            if let Some(start) = self.started_at {
                return since(start, now);
            }
            if let Some((ms, Some(at))) = self.reported_runtime {
                return since(at, now).map(|d| ms + d);
            }
        }
        if let Some((ms, _)) = self.reported_runtime {
            return Some(ms);
        }
        match (self.started_at, self.last_event_at) {
            (Some(start), Some(last)) => since(start, last),
            _ => None,
        }
    }
}

/// Folded observation state, fed incrementally.
#[derive(Debug, Default)]
pub struct Fold {
    /// Keyed by (session, agent); an event without `agentId` is the
    /// session's main agent.
    agents: BTreeMap<(String, String), AgentView>,
    pub events: u64,
    pub invalid: u64,
}

impl Fold {
    /// Feed one JSON line. Blank lines are ignored, unparsable ones counted.
    pub fn feed_line(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let Ok(record) = serde_json::from_str::<Record>(line) else {
            self.invalid += 1;
            return;
        };
        let session = record.session_id.clone().unwrap_or_default();
        let agent = record
            .agent_id
            .clone()
            .unwrap_or_else(|| format!("main:{session}"));
        let view = self
            .agents
            .entry((session, agent))
            .or_insert_with(|| AgentView {
                state: "unknown".into(),
                ..AgentView::default()
            });
        view.apply(record);
        self.events += 1;
    }

    pub fn feed(&mut self, text: &str) {
        for line in text.lines() {
            self.feed_line(line);
        }
    }

    /// All agents, with `runtime_ms` evaluated at `now`.
    pub fn views(&self, now: i64) -> Vec<AgentView> {
        self.agents
            .values()
            .map(|v| AgentView {
                runtime_ms: v.runtime_at(now),
                ..v.clone()
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Timestamps (RFC 3339 strings or epoch numbers), without a date crate
// ---------------------------------------------------------------------------

/// Epoch milliseconds from an RFC 3339 string or an epoch number (seconds
/// or milliseconds).
fn parse_ts(v: &serde_json::Value) -> Option<i64> {
    match v {
        serde_json::Value::Number(n) => {
            let n = n.as_f64()?;
            Some(if n > 1e11 {
                n as i64
            } else {
                (n * 1000.0) as i64
            })
        }
        serde_json::Value::String(s) => parse_rfc3339(s),
        _ => None,
    }
}

fn parse_rfc3339(s: &str) -> Option<i64> {
    let s = s.trim();
    let num = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    if s.len() < 19 || !matches!(s.as_bytes().get(10), Some(b'T' | b't' | b' ')) {
        return None;
    }
    let (year, month, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, min, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut rest = &s[19..];
    let mut millis = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(char::is_ascii_digit).collect();
        rest = &frac[digits.len()..];
        let padded = format!("{:0<3}", &digits[..digits.len().min(3)]);
        millis = padded.parse().ok()?;
    }
    let offset_min = match rest {
        "" | "Z" | "z" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let hh: i64 = rest.get(1..3)?.parse().ok()?;
            let mm: i64 = rest.get(rest.len() - 2..)?.parse().ok()?;
            sign * (hh * 60 + mm)
        }
    };
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3600 + min * 60 + sec - offset_min * 60;
    Some(secs * 1000 + millis)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn fmt_duration(ms: u64) -> String {
    let s = ms / 1000;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
    }
}

fn glyph(state: &str) -> console::StyledObject<&'static str> {
    match state {
        "working" => style("●").green(),
        "waiting" => style("◐").yellow(),
        "done" => style("✓").dim(),
        "failed" => style("✗").red(),
        _ => style("?").dim(),
    }
}

fn state_label(state: &str) -> console::StyledObject<String> {
    let text = format!("{state:<8}");
    match state {
        "working" => style(text).green(),
        "waiting" => style(text).yellow(),
        "failed" => style(text).red(),
        _ => style(text).dim(),
    }
}

fn short(id: &str, n: usize) -> String {
    id.chars().take(n).collect()
}

/// Display name of an agent within its session.
fn agent_name(v: &AgentView) -> String {
    match (v.role.as_str(), v.agent_id.as_deref()) {
        ("main", _) | (_, None) => "main".into(),
        (_, Some(id)) => short(id, 14),
    }
}

/// Width of indent plus agent name.
const NAME_COLUMN: usize = 18;

fn agent_line(v: &AgentView, prefix: &str, now: i64) -> String {
    let tool = if v.is_working() {
        v.tool.as_deref().unwrap_or("-")
    } else {
        "-"
    };
    let runtime = v
        .runtime_at(now)
        .map(fmt_duration)
        .unwrap_or_else(|| "-".into());
    let model = v.model.as_deref().unwrap_or("");
    let task = v.task.as_deref().unwrap_or("").replace(['\n', '\r'], " ");
    // Keep the columns aligned whatever the tree indent.
    let name_width = NAME_COLUMN.saturating_sub(prefix.chars().count()).max(8);
    format!(
        "{prefix}{} {:<name_width$} {} {:<12} {:>7}  {:<10} {}",
        glyph(&v.state),
        agent_name(v),
        state_label(&v.state),
        short(tool, 12),
        runtime,
        short(model, 10),
        style(task).dim()
    )
}

/// Counts over the given agents: (total, working, waiting, done, failed).
fn counts(views: &[&AgentView]) -> [usize; 5] {
    let n = |s: &str| views.iter().filter(|v| v.state == s).count();
    [
        views.len(),
        n("working"),
        n("waiting"),
        n("done"),
        n("failed"),
    ]
}

/// Render the fleet view as lines. `all` includes long-finished sessions.
pub fn render(views: &[AgentView], now: i64, all: bool, source: &str) -> Vec<String> {
    // Group per session, most recent activity first.
    let mut sessions: BTreeMap<&str, Vec<&AgentView>> = BTreeMap::new();
    for v in views {
        sessions
            .entry(v.session_id.as_deref().unwrap_or(""))
            .or_default()
            .push(v);
    }
    let mut sessions: Vec<(&str, Vec<&AgentView>)> = sessions
        .into_iter()
        .filter(|(_, agents)| {
            all || !agents.iter().all(|a| {
                a.is_finished()
                    && a.last_event_at
                        .is_none_or(|t| now - t > FINISHED_SESSION_TTL_MS)
            })
        })
        .collect();
    let last = |agents: &[&AgentView]| agents.iter().filter_map(|a| a.last_event_at).max();
    sessions.sort_by_key(|s| std::cmp::Reverse(last(&s.1)));

    let shown: Vec<&AgentView> = sessions
        .iter()
        .flat_map(|(_, a)| a.iter().copied())
        .collect();
    let [total, working, waiting, done, failed] = counts(&shown);
    let mut out = vec![format!(
        "{}  {total} agent(s) · {} working · {} waiting · {done} done · {} failed",
        style("Agents").bold(),
        style(working).green(),
        style(waiting).yellow(),
        style(failed).red()
    )];
    if sessions.is_empty() {
        out.push(String::new());
        out.push(format!(
            "  {}",
            style("No active agents (finished sessions hidden, use --all).").dim()
        ));
    }

    for (session, agents) in &sessions {
        out.push(String::new());
        let first = |f: fn(&AgentView) -> Option<&String>| {
            agents
                .iter()
                .find(|a| a.role == "main")
                .and_then(|a| f(a))
                .or_else(|| agents.iter().find_map(|a| f(a)))
                .cloned()
        };
        let repo = first(|a| a.repository.as_ref());
        let worktree = first(|a| a.worktree.as_ref());
        let location = match (repo, worktree) {
            (Some(r), Some(w)) => format!("{r} @ {w}"),
            (Some(r), None) => r,
            (None, Some(w)) => format!("@ {w}"),
            (None, None) => String::new(),
        };
        let label = if session.is_empty() {
            "session (unknown)".to_string()
        } else {
            format!("session {}", short(session, 8))
        };
        out.push(format!("{}  {}", style(label).bold().cyan(), location));

        let background: Vec<&&AgentView> =
            agents.iter().filter(|a| a.role == "background").collect();
        let tree: Vec<&AgentView> = agents
            .iter()
            .copied()
            .filter(|a| a.role != "background")
            .collect();
        let known = |id: &Option<String>| {
            id.as_ref()
                .is_some_and(|p| tree.iter().any(|a| a.agent_id.as_ref() == Some(p)))
        };
        let roots: Vec<&AgentView> = tree
            .iter()
            .copied()
            .filter(|a| !known(&a.parent_agent_id))
            .collect();
        for root in &roots {
            out.push(agent_line(root, "  ", now));
            push_children(&mut out, &tree, root, "  ", now, 0);
        }
        if !background.is_empty() {
            out.push(format!("  {}", style("background").dim()));
            for b in background {
                out.push(agent_line(b, "    ", now));
            }
        }
    }
    out.push(String::new());
    out.push(format!("{}", style(source).dim()));
    out
}

fn push_children(
    out: &mut Vec<String>,
    tree: &[&AgentView],
    parent: &AgentView,
    indent: &str,
    now: i64,
    depth: usize,
) {
    if depth > 8 || parent.agent_id.is_none() {
        return;
    }
    let children: Vec<&AgentView> = tree
        .iter()
        .copied()
        .filter(|a| a.parent_agent_id.is_some() && a.parent_agent_id == parent.agent_id)
        .collect();
    for (i, child) in children.iter().enumerate() {
        let last = i + 1 == children.len();
        let branch = if last { "└ " } else { "├ " };
        out.push(agent_line(child, &format!("{indent}{branch}"), now));
        let next = format!("{indent}{}", if last { "  " } else { "│ " });
        push_children(out, tree, child, &next, now, depth + 1);
    }
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// The events file: `--file`, else `<root>/.nexus/claude/observer/events.jsonl`
/// for the nearest directory upwards that has a `.nexus/` directory.
pub fn events_path(file: Option<&str>) -> anyhow::Result<PathBuf> {
    if let Some(f) = file {
        return Ok(PathBuf::from(f));
    }
    let cwd = std::env::current_dir()?;
    let root = cwd
        .ancestors()
        .find(|d| d.join(".nexus").is_dir())
        .unwrap_or(&cwd)
        .to_path_buf();
    let agentic_root = super::run::resolve_agentic_root(&root);
    Ok(root.join(agentic_root).join(EVENTS_REL))
}

fn display_path(path: &Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| path.strip_prefix(&cwd).ok().map(Path::to_path_buf))
        .unwrap_or_else(|| path.to_path_buf())
        .display()
        .to_string()
}

fn no_data_lines(path: &str) -> Vec<String> {
    vec![
        format!("{}  no observation data", style("Agents").bold()),
        String::new(),
        format!("  {path} not found."),
        format!(
            "  {}",
            style(
                "It is written by the Nexus Claude Code hook once installed (runtime plugins, via nexus pull)."
            )
            .dim()
        ),
    ]
}

fn source_line(path: &str, fold: &Fold) -> String {
    let mut s = format!("{path} · {} event(s)", fold.events);
    if fold.invalid > 0 {
        s.push_str(&format!(" · {} invalid line(s) skipped", fold.invalid));
    }
    s
}

/// `nexus status --agents [--json] [--all] [--file <path>]`, one shot.
pub fn show(file: Option<&str>, json: bool, all: bool) -> anyhow::Result<()> {
    let path = events_path(file)?;
    let shown = display_path(&path);
    let text = match std::fs::read(&path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if json {
                println!("[]");
            } else {
                for line in no_data_lines(&shown) {
                    println!("{line}");
                }
            }
            return Ok(());
        }
        Err(e) => return Err(anyhow::anyhow!("cannot read {shown}: {e}")),
    };
    let mut fold = Fold::default();
    fold.feed(&text);
    let now = now_ms();
    if json {
        println!("{}", serde_json::to_string_pretty(&fold.views(now))?);
    } else {
        for line in render(&fold.views(now), now, all, &source_line(&shown, &fold)) {
            println!("{line}");
        }
    }
    Ok(())
}

/// Incremental reader for an append-only file that may be rotated or
/// truncated.
#[derive(Debug, Default)]
pub struct Follower {
    offset: u64,
    identity: Option<(u64, u64)>,
    partial: String,
}

impl Follower {
    /// New complete lines since the last call. `None` when the file does not
    /// exist. Rotation (a new file) and truncation restart at offset 0; the
    /// folded state is kept, since the old content was already read.
    pub fn poll(&mut self, path: &Path) -> Option<String> {
        let mut file = std::fs::File::open(path).ok()?;
        let meta = file.metadata().ok()?;
        let identity = file_identity(&meta);
        if self.identity.is_some() && self.identity != identity {
            self.offset = 0;
            self.partial.clear();
        }
        self.identity = identity;
        if meta.len() < self.offset {
            self.offset = 0;
            self.partial.clear();
        }
        if meta.len() == self.offset {
            return Some(String::new());
        }
        file.seek(SeekFrom::Start(self.offset)).ok()?;
        let mut buf = Vec::new();
        file.read_to_end(&mut buf).ok()?;
        self.offset += buf.len() as u64;
        self.partial.push_str(&String::from_utf8_lossy(&buf));
        match self.partial.rfind('\n') {
            Some(end) => {
                let complete = self.partial[..=end].to_string();
                self.partial.drain(..=end);
                Some(complete)
            }
            None => Some(String::new()),
        }
    }
}

#[cfg(unix)]
fn file_identity(meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev(), meta.ino()))
}

#[cfg(not(unix))]
fn file_identity(_meta: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

/// `nexus status --agents --watch`: follow the file and redraw once per
/// second until interrupted. With `--json`, prints one compact DTO array per
/// change instead.
pub fn watch(file: Option<&str>, json: bool, all: bool) -> anyhow::Result<()> {
    let path = events_path(file)?;
    let shown = display_path(&path);
    let mut fold = Fold::default();
    let mut follower = Follower::default();
    let term = console::Term::stdout();
    let mut first = true;
    loop {
        let chunk = follower.poll(&path);
        let changed = chunk.as_deref().is_some_and(|c| !c.is_empty());
        if let Some(ref c) = chunk {
            fold.feed(c);
        }
        let now = now_ms();
        if json {
            if changed || first {
                println!("{}", serde_json::to_string(&fold.views(now))?);
            }
        } else {
            let lines = match chunk {
                None => no_data_lines(&shown),
                Some(_) => render(&fold.views(now), now, all, &source_line(&shown, &fold)),
            };
            draw(&term, &lines)?;
        }
        first = false;
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Redraw in place: home, each line cleared to its end, rest of the screen
/// cleared. Lines are cut to the terminal size.
fn draw(term: &console::Term, lines: &[String]) -> anyhow::Result<()> {
    let (rows, cols) = term.size();
    let mut out = String::from("\x1b[H");
    for line in lines.iter().take(rows.saturating_sub(1).max(1) as usize) {
        out.push_str(&console::truncate_str(line, cols as usize, "…"));
        out.push_str("\x1b[K\n");
    }
    out.push_str("\x1b[J");
    let mut stdout = std::io::stdout().lock();
    stdout.write_all(out.as_bytes())?;
    stdout.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: &str = "2026-09-26T10:00:00Z";

    fn line(v: serde_json::Value) -> String {
        v.to_string()
    }

    #[test]
    fn test_parse_ts_forms() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(parse_rfc3339("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(
            parse_rfc3339("2026-09-26T10:00:00.123456Z"),
            Some(1_790_416_800_123)
        );
        assert_eq!(parse_rfc3339("not a date"), None);
        assert_eq!(
            parse_ts(&serde_json::json!(1_790_416_800_000i64)),
            Some(1_790_416_800_000)
        );
        assert_eq!(
            parse_ts(&serde_json::json!(1_790_416_800)),
            Some(1_790_416_800_000)
        );
    }

    fn sample() -> Fold {
        let mut f = Fold::default();
        f.feed(&[
            line(serde_json::json!({"ts": T0, "event": "SessionStart", "sessionId": "s1", "agentId": "a-main", "role": "main", "state": "working", "model": "opus", "repository": "nexus-cli", "worktree": "main", "startedAt": T0, "task": "implement"})),
            line(serde_json::json!({"ts": "2026-09-26T10:00:10Z", "event": "PreToolUse", "sessionId": "s1", "agentId": "a-main", "tool": "Bash", "state": "working", "model": null})),
            line(serde_json::json!({"ts": "2026-09-26T10:00:20Z", "event": "SubagentStart", "sessionId": "s1", "agentId": "sub-1", "parentAgentId": "a-main", "state": "working", "tool": "Grep", "startedAt": "2026-09-26T10:00:20Z"})),
            line(serde_json::json!({"ts": "2026-09-26T10:00:30Z", "event": "SubagentStop", "sessionId": "s1", "agentId": "sub-2", "parentAgentId": "a-main", "state": "done", "runtimeMs": 5000})),
            line(serde_json::json!({"ts": "2026-09-26T10:00:40Z", "event": "PreToolUse", "sessionId": "s1", "agentId": "bg-1", "role": "background", "state": "working", "task": "npm run dev"})),
            "{broken".to_string(),
            String::new(),
        ].join("\n"));
        f
    }

    #[test]
    fn test_fold_merges_events_per_agent() {
        let f = sample();
        assert_eq!(f.events, 5);
        assert_eq!(f.invalid, 1);
        let now = parse_rfc3339("2026-09-26T10:01:00Z").unwrap();
        let views = f.views(now);
        assert_eq!(views.len(), 4);
        let main = views
            .iter()
            .find(|v| v.agent_id.as_deref() == Some("a-main"))
            .unwrap();
        // A null in a later event does not erase a known value.
        assert_eq!(main.model.as_deref(), Some("opus"));
        assert_eq!(main.tool.as_deref(), Some("Bash"));
        assert_eq!(main.role, "main");
        assert_eq!(main.events, 2);
        assert_eq!(main.last_event.as_deref(), Some("PreToolUse"));
        // Working: counts up from startedAt.
        assert_eq!(main.runtime_ms, Some(60_000));
        let sub = views
            .iter()
            .find(|v| v.agent_id.as_deref() == Some("sub-1"))
            .unwrap();
        assert_eq!(sub.role, "subagent");
        assert_eq!(sub.runtime_ms, Some(40_000));
        let done = views
            .iter()
            .find(|v| v.agent_id.as_deref() == Some("sub-2"))
            .unwrap();
        assert_eq!(done.runtime_ms, Some(5000));
        let bg = views
            .iter()
            .find(|v| v.agent_id.as_deref() == Some("bg-1"))
            .unwrap();
        assert_eq!(bg.role, "background");
        assert_eq!(bg.state, "working");
    }

    #[test]
    fn test_event_without_agent_id_is_session_main() {
        let mut f = Fold::default();
        f.feed_line(r#"{"sessionId":"s9","state":"waiting"}"#);
        f.feed_line(r#"{"sessionId":"s9","state":"working","tool":"Edit"}"#);
        let views = f.views(0);
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].role, "main");
        assert_eq!(views[0].state, "working");
        // No timing information: unknown runtime.
        assert_eq!(views[0].runtime_ms, None);
    }

    #[test]
    fn test_render_tree_and_counts() {
        let f = sample();
        let now = parse_rfc3339("2026-09-26T10:01:00Z").unwrap();
        let lines: Vec<String> = render(&f.views(now), now, false, "src")
            .iter()
            .map(|l| console::strip_ansi_codes(l).into_owned())
            .collect();
        let text = lines.join("\n");
        assert!(
            text.contains("4 agent(s) · 3 working · 0 waiting · 1 done · 0 failed"),
            "{text}"
        );
        assert!(text.contains("session s1  nexus-cli @ main"), "{text}");
        let main = lines.iter().position(|l| l.contains("● main")).unwrap();
        assert!(
            lines[main].contains("Bash") && lines[main].contains("1m00s"),
            "{text}"
        );
        assert!(lines[main + 1].starts_with("  ├ ● sub-1"), "{text}");
        assert!(lines[main + 2].starts_with("  └ ✓ sub-2"), "{text}");
        // A finished agent shows no current tool.
        assert!(!lines[main + 2].contains("Grep"));
        assert!(text.contains("background"));
        assert!(text.contains("bg-1") && text.contains("npm run dev"));
    }

    #[test]
    fn test_render_hides_long_finished_sessions() {
        let mut f = Fold::default();
        f.feed_line(&line(
            serde_json::json!({"ts": T0, "sessionId": "old", "state": "done"}),
        ));
        let now = parse_rfc3339("2026-09-26T12:00:00Z").unwrap();
        let hidden = render(&f.views(now), now, false, "src").join("\n");
        assert!(!hidden.contains("session old"));
        assert!(console::strip_ansi_codes(&hidden).contains("No active agents"));
        let all = render(&f.views(now), now, true, "src").join("\n");
        assert!(all.contains("session old"));
    }

    #[test]
    fn test_follower_incremental_partial_and_rotation() {
        let dir = std::env::temp_dir().join(format!("nexus-observer-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("events.jsonl");
        let mut follower = Follower::default();
        assert_eq!(follower.poll(&path), None);

        std::fs::write(&path, "{\"a\":1}\n{\"b\"").unwrap();
        assert_eq!(follower.poll(&path).unwrap(), "{\"a\":1}\n");
        assert_eq!(follower.poll(&path).unwrap(), "");
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap();
        f.write_all(b":2}\n").unwrap();
        assert_eq!(follower.poll(&path).unwrap(), "{\"b\":2}\n");

        // Rotation: the file is moved away and a new one starts.
        std::fs::rename(&path, dir.join("events.jsonl.1")).unwrap();
        std::fs::write(&path, "{\"c\":3}\n").unwrap();
        assert_eq!(follower.poll(&path).unwrap(), "{\"c\":3}\n");

        // Truncation in place.
        std::fs::write(&path, "").unwrap();
        assert_eq!(follower.poll(&path).unwrap(), "");
        std::fs::write(&path, "{\"d\":4}\n").unwrap();
        assert_eq!(follower.poll(&path).unwrap(), "{\"d\":4}\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_fmt_duration() {
        assert_eq!(fmt_duration(5_000), "5s");
        assert_eq!(fmt_duration(65_000), "1m05s");
        assert_eq!(fmt_duration(3_725_000), "1h02m");
    }
}
