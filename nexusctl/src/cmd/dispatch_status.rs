//! `nexus dispatch status`: a read-only view of a project's Dispatch traffic
//! (incoming, outgoing, what waits on us), for the terminal and as a narrow
//! pane in the `nexus run` workspace (`nexus run --dispatch-pane`).
//!
//! Everything comes from `dispatch_inbox` / `dispatch_outbox` on
//! `POST /api/mcp/dispatches` with the user's token and is computed client
//! side. The hub returns at most 50 rows per call and has no cursor, so a
//! count from a full page is shown as `50+`. The command holds no lock: the
//! runner file is only read, to show its state.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::time::Duration;

use console::style;
use nexus_core::api::{DispatchSummary, NexusClient, DISPATCH_LIST_LIMIT};
use nexus_core::auth::resolve_token;
use nexus_core::config;
use serde::Serialize;

use super::observer::{days_from_civil, draw, now_ms, parse_rfc3339};
use super::preflight::CheckResult;

/// Statuses of a Dispatch that still needs someone.
const ACTIVE: &[&str] = &[
    "open",
    "acknowledged",
    "in_progress",
    "waiting_for_requester",
    "blocked",
    "needs_review",
];
const FINISHED: &[&str] = &["resolved", "closed"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum Direction {
    In,
    Out,
}

impl Direction {
    fn label(self) -> &'static str {
        match self {
            Direction::In => "in",
            Direction::Out => "out",
        }
    }
}

/// Whether the next step of a Dispatch is ours: incoming that we have not
/// taken up or that is blocked, outgoing that waits for us as requester.
fn waits_on_us(dir: Direction, status: &str) -> bool {
    match dir {
        Direction::In => matches!(status, "open" | "acknowledged" | "blocked"),
        Direction::Out => matches!(status, "waiting_for_requester" | "needs_review"),
    }
}

/// A count and whether it came from a full page (`50+`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
struct Count {
    value: usize,
    truncated: bool,
}

impl Count {
    fn plus(self, other: Count) -> Count {
        Count {
            value: self.value + other.value,
            truncated: self.truncated || other.truncated,
        }
    }

    fn show(self) -> String {
        if self.truncated {
            format!("{}+", self.value)
        } else {
            self.value.to_string()
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
struct Incoming {
    open: Count,
    waiting_on_us: Count,
    in_progress: Count,
    blocked: Count,
    resolved: Count,
    closed: Count,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
struct Outgoing {
    open: Count,
    waiting_on_us: Count,
    waiting_on_peer: Count,
    resolved: Count,
    closed: Count,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Row {
    direction: Direction,
    id: String,
    peer_project_id: String,
    peer: String,
    title: String,
    #[serde(rename = "type")]
    kind: String,
    priority: String,
    blocking: bool,
    status: String,
    created_at: String,
    updated_at: Option<String>,
    waiting_on_us: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Peer {
    project_id: String,
    name: String,
    incoming: usize,
    outgoing: usize,
    waiting_on_us: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct ProjectRef {
    id: String,
    name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Runner {
    state: String,
    /// Why the runner of a watching view is not listening, or what it
    /// last failed at.
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
    /// The runner's last error from its state file.
    #[serde(skip_serializing_if = "Option::is_none")]
    last_error: Option<String>,
}

/// The `--json` shape (`nexus.dispatch-status.v1`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct Snapshot {
    schema: &'static str,
    project: ProjectRef,
    refreshed_at: String,
    runner: Runner,
    window_days: u32,
    incoming: Incoming,
    outgoing: Outgoing,
    peers: Vec<Peer>,
    dispatches: Vec<Row>,
    /// The last refresh failed; the rest is the last good state.
    error: Option<String>,
}

/// The four lists one refresh reads.
struct Lists {
    inbox: Vec<DispatchSummary>,
    outbox: Vec<DispatchSummary>,
    inbox_done: Vec<DispatchSummary>,
    outbox_done: Vec<DispatchSummary>,
}

fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

fn full(list: &[DispatchSummary]) -> bool {
    list.len() >= DISPATCH_LIST_LIMIT
}

/// Count rows matching `pred`; a full page makes the count a lower bound.
fn count(list: &[DispatchSummary], pred: impl Fn(&DispatchSummary) -> bool) -> Count {
    Count {
        value: list.iter().filter(|d| pred(d)).count(),
        truncated: full(list),
    }
}

/// Finished within the window: resolved by `resolved_at`, closed by `closed_at`.
fn finished(list: &[DispatchSummary], status: &str, since_ms: i64) -> Count {
    count(list, |d| {
        let at = match status {
            "resolved" => d.resolved_at.as_deref(),
            _ => d.closed_at.as_deref(),
        };
        d.status == status && at.and_then(parse_rfc3339).is_some_and(|t| t >= since_ms)
    })
}

/// RFC 3339 UTC timestamp for now.
pub(crate) fn rfc3339_now() -> String {
    rfc3339(now_ms())
}

/// RFC 3339 UTC timestamp for epoch milliseconds.
pub(crate) fn rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Inverse of `days_from_civil` (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    debug_assert_eq!(days_from_civil(y, m, d), z - 719_468);
    (y, m, d)
}

/// Build the snapshot from one refresh's lists.
fn snapshot(
    project: ProjectRef,
    names: &BTreeMap<String, String>,
    lists: &Lists,
    runner: Runner,
    window_days: u32,
    now: i64,
) -> Snapshot {
    let since = now - i64::from(window_days) * 86_400_000;
    let active = |d: &&DispatchSummary| ACTIVE.contains(&d.status.as_str());
    let name_of = |id: &str| names.get(id).cloned().unwrap_or_else(|| short_id(id));

    let mut rows: Vec<Row> = lists
        .inbox
        .iter()
        .filter(active)
        .map(|d| (Direction::In, d, &d.source_project_id))
        .chain(
            lists
                .outbox
                .iter()
                .filter(active)
                .map(|d| (Direction::Out, d, &d.target_project_id)),
        )
        .map(|(direction, d, peer)| Row {
            direction,
            id: d.id.clone(),
            peer_project_id: peer.clone(),
            peer: name_of(peer),
            title: d.title.clone(),
            kind: d.kind.clone(),
            priority: d.priority.clone(),
            blocking: d.blocking,
            status: d.status.clone(),
            created_at: d.created_at.clone(),
            updated_at: d.updated_at.clone(),
            waiting_on_us: waits_on_us(direction, &d.status),
        })
        .collect();
    // Ours first, then blocking, then the most recent activity.
    rows.sort_by(|a, b| {
        let last = |r: &Row| {
            r.updated_at
                .as_deref()
                .or(Some(&r.created_at))
                .and_then(parse_rfc3339)
                .unwrap_or(0)
        };
        b.waiting_on_us
            .cmp(&a.waiting_on_us)
            .then(b.blocking.cmp(&a.blocking))
            .then(last(b).cmp(&last(a)))
    });

    let mut peers: BTreeMap<String, Peer> = BTreeMap::new();
    for r in &rows {
        let p = peers
            .entry(r.peer_project_id.clone())
            .or_insert_with(|| Peer {
                project_id: r.peer_project_id.clone(),
                name: r.peer.clone(),
                incoming: 0,
                outgoing: 0,
                waiting_on_us: 0,
            });
        match r.direction {
            Direction::In => p.incoming += 1,
            Direction::Out => p.outgoing += 1,
        }
        p.waiting_on_us += usize::from(r.waiting_on_us);
    }
    let mut peers: Vec<Peer> = peers.into_values().collect();
    peers.sort_by(|a, b| {
        b.waiting_on_us
            .cmp(&a.waiting_on_us)
            .then((b.incoming + b.outgoing).cmp(&(a.incoming + a.outgoing)))
            .then(a.name.cmp(&b.name))
    });

    let (inbox, outbox) = (&lists.inbox, &lists.outbox);
    Snapshot {
        schema: "nexus.dispatch-status.v1",
        project,
        refreshed_at: rfc3339(now),
        runner,
        window_days,
        incoming: Incoming {
            open: count(inbox, |d| d.status == "open"),
            waiting_on_us: count(inbox, |d| waits_on_us(Direction::In, &d.status)),
            in_progress: count(inbox, |d| d.status == "in_progress"),
            blocked: count(inbox, |d| d.status == "blocked"),
            resolved: finished(&lists.inbox_done, "resolved", since),
            closed: finished(&lists.inbox_done, "closed", since),
        },
        outgoing: Outgoing {
            open: count(outbox, |d| d.status == "open"),
            waiting_on_us: count(outbox, |d| waits_on_us(Direction::Out, &d.status)),
            waiting_on_peer: count(outbox, |d| {
                ACTIVE.contains(&d.status.as_str()) && !waits_on_us(Direction::Out, &d.status)
            }),
            resolved: finished(&lists.outbox_done, "resolved", since),
            closed: finished(&lists.outbox_done, "closed", since),
        },
        peers,
        dispatches: rows,
        error: None,
    }
}

/// Runner state from `~/.config/nexus/state/dispatch/hubs/<hub>/runner.json`, read
/// only; `none` while no runner has written it.
fn runner_state(path: &Path) -> Runner {
    let mut last_error = None;
    let state = match fs::read_to_string(path) {
        Err(_) => "none".to_string(),
        Ok(body) => match serde_json::from_str::<serde_json::Value>(&body) {
            Err(_) => "unknown".to_string(),
            Ok(v) => {
                last_error = v
                    .get("last_error")
                    .and_then(|e| e.as_str())
                    .map(str::to_string);
                let state = v
                    .get("state")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown")
                    .to_string();
                let pid = v.get("pid").and_then(|p| p.as_u64()).unwrap_or(0) as u32;
                // A runner killed without a chance to write "stopped".
                let live = matches!(state.as_str(), "starting" | "running" | "blocked" | "error");
                if live && !process_alive(pid) {
                    "stopped".to_string()
                } else {
                    state
                }
            }
        },
    };
    Runner {
        state,
        note: None,
        last_error,
    }
}

/// The runner as this project sees it: a runner serving only other
/// projects reads `elsewhere` and names them; a bound runner shows its last
/// error unless a stop reason is already noted.
fn runner_view(mut r: Runner, scope: &super::dispatch_listen::RunnerScope) -> Runner {
    use super::dispatch_listen::RunnerScope;
    let live = matches!(
        r.state.as_str(),
        "starting" | "running" | "blocked" | "error"
    );
    match scope {
        RunnerScope::Unbound(names) if live => {
            r.state = "elsewhere".into();
            r.note = Some(format!(
                "serves {}, not this project; run: {}",
                names.join(", "),
                super::dispatch_listen::REGISTER_CMD
            ));
        }
        RunnerScope::Bound if live && r.note.is_none() => {
            r.note = r.last_error.as_ref().map(|e| format!("last error: {e}"));
        }
        _ => {}
    }
    r
}

fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks that the process exists.
        let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
        rc == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// The listener state of `api_url`'s hub.
fn runner_path(api_url: &str) -> Option<std::path::PathBuf> {
    super::dispatch_runner::hub_paths(api_url)
        .ok()
        .map(|p| p.hub_dir.join("runner.json"))
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// `12s`, `5m`, `3h`, `2d`.
fn age(from: Option<&str>, now: i64) -> String {
    let Some(t) = from.and_then(parse_rfc3339) else {
        return "?".into();
    };
    let s = ((now - t).max(0) / 1000) as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m", s / 60),
        3600..=172_799 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86_400),
    }
}

/// Short status for the compact view.
fn status_short(status: &str) -> &str {
    match status {
        "acknowledged" => "ack",
        "in_progress" => "prog",
        "waiting_for_requester" => "wait",
        "needs_review" => "revw",
        "blocked" => "blkd",
        other => other,
    }
}

/// Pad or cut to exactly `width` characters.
fn fit(s: &str, width: usize) -> String {
    // `truncate_str` also cuts a string of exactly `width` characters.
    if console::measure_text_width(s) <= width {
        return format!("{s:<width$}");
    }
    let cut = console::truncate_str(s, width, "…");
    format!("{cut:<width$}")
}

/// Color of a counter or status. Shown only on a color terminal; pipes,
/// `NO_COLOR` and `--json` stay plain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tone {
    Ours,
    Blocked,
    Progress,
    Open,
    Peer,
    Resolved,
    Closed,
}

fn tone_style(text: String, tone: Tone) -> console::StyledObject<String> {
    let s = style(text);
    match tone {
        Tone::Ours => s.yellow().bold(),
        Tone::Blocked => s.red(),
        Tone::Progress => s.blue(),
        Tone::Open => s.cyan(),
        Tone::Peer => s,
        Tone::Resolved => s.green(),
        Tone::Closed => s.dim(),
    }
}

/// `<count> <label>` in its tone; dimmed when zero (also `0+`), so
/// non-zero values stand out.
fn counter_style(c: Count, label: &str, tone: Tone) -> console::StyledObject<String> {
    let text = format!("{} {label}", c.show());
    if c.value == 0 {
        style(text).dim()
    } else {
        tone_style(text, tone)
    }
}

fn counter(c: Count, label: &str, tone: Tone) -> String {
    counter_style(c, label, tone).to_string()
}

/// A row's status tone: blocked first, then our turn, then the state.
fn status_tone(r: &Row) -> Tone {
    match r.status.as_str() {
        "blocked" => Tone::Blocked,
        _ if r.waiting_on_us => Tone::Ours,
        "in_progress" => Tone::Progress,
        "open" => Tone::Open,
        _ => Tone::Peer,
    }
}

fn blocking_mark(r: &Row) -> String {
    if r.blocking {
        style("!").red().bold().to_string()
    } else {
        " ".into()
    }
}

/// `<n> <label>` for the per-peer lines: highlighted when something waits on us.
fn ours_count(n: usize, label: &str) -> String {
    counter(
        Count {
            value: n,
            truncated: false,
        },
        label,
        Tone::Ours,
    )
}

/// Test traffic from the hub's simulation form carries this title prefix.
const SIMULATION_PREFIX: &str = "[Simulation]";

/// The pane's name for the Dispatch runner of this machine.
const MANAGER: &str = "manager";

fn is_simulation(r: &Row) -> bool {
    r.title.trim_start().starts_with(SIMULATION_PREFIX)
}

/// Plain row text, yellow for a simulation Dispatch.
fn sim(r: &Row, text: String) -> String {
    if is_simulation(r) {
        style(text).yellow().to_string()
    } else {
        text
    }
}

fn marker(r: &Row) -> String {
    if r.waiting_on_us {
        style(">").bold().yellow().to_string()
    } else {
        " ".into()
    }
}

fn hhmm(refreshed_at: &str) -> &str {
    refreshed_at.get(11..19).unwrap_or(refreshed_at)
}

/// An error with its causes, skipping a cause already in the text
/// (`HTTP error: ...` repeats the transport message).
pub(crate) fn error_text(e: &(dyn std::error::Error + 'static)) -> String {
    let mut text = e.to_string();
    let mut source = e.source();
    while let Some(cause) = source {
        let msg = cause.to_string();
        if !text.contains(&msg) {
            text.push_str(": ");
            text.push_str(&msg);
        }
        source = cause.source();
    }
    text
}

fn runner_note_line(snap: &Snapshot) -> Option<String> {
    snap.runner
        .note
        .as_ref()
        .map(|n| style(format!("{MANAGER}: {n}")).yellow().to_string())
}

fn error_line(snap: &Snapshot) -> Option<String> {
    snap.error.as_ref().map(|e| {
        style(format!("! refresh failed, showing last state: {e}"))
            .red()
            .to_string()
    })
}

fn render(snap: &Snapshot, now: i64) -> Vec<String> {
    let (i, o) = (&snap.incoming, &snap.outgoing);
    let mut lines = vec![
        format!(
            "{}  {}   refreshed {}Z   {MANAGER}: {}",
            style("Dispatches").bold(),
            style(&snap.project.name).bold(),
            hhmm(&snap.refreshed_at),
            snap.runner.state
        ),
        format!(
            "  in   {} · {} · {} · {}",
            counter(i.open, "open", Tone::Open),
            counter(i.waiting_on_us, "waiting on us", Tone::Ours),
            counter(i.in_progress, "in progress", Tone::Progress),
            counter(i.blocked, "blocked", Tone::Blocked)
        ),
        format!(
            "  out  {} · {} · {}",
            counter(o.open, "open", Tone::Open),
            counter(o.waiting_on_us, "waiting on us", Tone::Ours),
            counter(o.waiting_on_peer, "waiting on peer", Tone::Peer)
        ),
        format!(
            "  {}d   in {} · {}   out {} · {}",
            snap.window_days,
            counter(i.resolved, "resolved", Tone::Resolved),
            counter(i.closed, "closed", Tone::Closed),
            counter(o.resolved, "resolved", Tone::Resolved),
            counter(o.closed, "closed", Tone::Closed)
        ),
    ];
    lines.extend(runner_note_line(snap));
    lines.extend(error_line(snap));
    lines.push(String::new());
    if snap.dispatches.is_empty() {
        lines.push("  No open Dispatches.".into());
        return lines;
    }
    lines.push(
        style(format!(
            "  {:<3} {:<16} {:<22} {:<7} {:<22} {:>4} {:>6}  TITLE",
            "DIR", "PEER", "TYPE", "PRIO", "STATUS", "AGE", "ACTIVE"
        ))
        .dim()
        .to_string(),
    );
    for r in &snap.dispatches {
        lines.push(format!(
            "{} {} {} {} {} {}{} {:>4} {:>6}  {}",
            marker(r),
            sim(r, format!("{:<3}", r.direction.label())),
            sim(r, fit(&r.peer, 16)),
            sim(r, fit(&r.kind, 22)),
            sim(r, fit(&r.priority, 7)),
            blocking_mark(r),
            tone_style(fit(&r.status, 21), status_tone(r)),
            age(Some(&r.created_at), now),
            age(r.updated_at.as_deref(), now),
            sim(r, r.title.clone())
        ));
    }
    lines.push(String::new());
    for p in &snap.peers {
        lines.push(format!(
            "  {} {} in / {} out, {}",
            fit(&p.name, 16),
            p.incoming,
            p.outgoing,
            ours_count(p.waiting_on_us, "waiting on us")
        ));
    }
    lines
}

/// The narrow pane view (readable at 45 columns): columns are dropped and
/// titles cut instead of wrapping.
fn render_compact(snap: &Snapshot, now: i64) -> Vec<String> {
    let (i, o) = (&snap.incoming, &snap.outgoing);
    let mut lines = vec![
        format!(
            "{} {}Z {MANAGER} {}",
            style(console::truncate_str(&snap.project.name, 20, "…")).bold(),
            &hhmm(&snap.refreshed_at)[..5.min(hhmm(&snap.refreshed_at).len())],
            snap.runner.state
        ),
        format!(
            "in  {} {} {} {}",
            counter(i.open, "open", Tone::Open),
            counter(i.waiting_on_us, "ours", Tone::Ours),
            counter(i.in_progress, "prog", Tone::Progress),
            counter(i.blocked, "blkd", Tone::Blocked)
        ),
        format!(
            "out {} {} {}",
            counter(o.open, "open", Tone::Open),
            counter(o.waiting_on_us, "ours", Tone::Ours),
            counter(o.waiting_on_peer, "peer", Tone::Peer)
        ),
        format!(
            "{}d  {} {}",
            snap.window_days,
            counter(i.resolved.plus(o.resolved), "resolved", Tone::Resolved),
            counter(i.closed.plus(o.closed), "closed", Tone::Closed)
        ),
    ];
    lines.extend(runner_note_line(snap));
    lines.extend(error_line(snap));
    if snap.dispatches.is_empty() {
        lines.push("no open Dispatches".into());
        return lines;
    }
    for r in &snap.dispatches {
        lines.push(format!(
            "{}{}{} {} {} {:>3} {}",
            marker(r),
            blocking_mark(r),
            sim(r, format!("{:<3}", r.direction.label())),
            sim(r, fit(&r.peer, 10)),
            tone_style(fit(status_short(&r.status), 4), status_tone(r)),
            age(r.updated_at.as_deref().or(Some(&r.created_at)), now),
            sim(r, r.title.clone())
        ));
    }
    for p in &snap.peers {
        lines.push(format!(
            "{} {}/{}, {}",
            fit(&p.name, 12),
            p.incoming,
            p.outgoing,
            ours_count(p.waiting_on_us, "ours")
        ));
    }
    lines
}

// ---------------------------------------------------------------------------
// Command
// ---------------------------------------------------------------------------

async fn fetch(client: &NexusClient, project_id: &str) -> anyhow::Result<Lists> {
    let (inbox, outbox, inbox_done, outbox_done) = tokio::try_join!(
        client.list_dispatches("dispatch_inbox", project_id, ACTIVE),
        client.list_dispatches("dispatch_outbox", project_id, ACTIVE),
        client.list_dispatches("dispatch_inbox", project_id, FINISHED),
        client.list_dispatches("dispatch_outbox", project_id, FINISHED),
    )?;
    Ok(Lists {
        inbox: inbox.dispatches,
        outbox: outbox.dispatches,
        inbox_done: inbox_done.dispatches,
        outbox_done: outbox_done.dispatches,
    })
}

/// Project names by id, from the project list; empty when it cannot be read.
async fn project_names(client: &NexusClient) -> BTreeMap<String, String> {
    client
        .list_projects()
        .await
        .map(|r| r.projects.into_iter().map(|p| (p.id, p.name)).collect())
        .unwrap_or_default()
}

pub struct StatusOptions<'a> {
    pub project_id: Option<&'a str>,
    pub watch: bool,
    pub interval_secs: u64,
    pub window_days: u32,
    pub compact: bool,
    pub json: bool,
    /// With --watch: run the Dispatch runner in this process (default).
    pub listen: bool,
}

/// `nexus dispatch status`.
pub async fn status(api_url: &str, opts: StatusOptions<'_>) -> anyhow::Result<()> {
    let workspace = std::env::current_dir()?;
    let project_id = config::resolve_project_id(opts.project_id, Some(&workspace))?;
    let token = resolve_token().ok_or_else(|| {
        anyhow::anyhow!("No authentication token found. Run 'nexus login' first.")
    })?;
    let client = NexusClient::new(api_url, Some(token))?;
    let runner_file = runner_path(api_url);
    let mut names = project_names(&client).await;
    let project_name = |names: &BTreeMap<String, String>| ProjectRef {
        id: project_id.clone(),
        name: names
            .get(&project_id)
            .cloned()
            .unwrap_or_else(|| short_id(&project_id)),
    };
    // With --watch the view also runs the runner, or says why it cannot.
    let blocker = (opts.listen && opts.watch)
        .then(|| super::dispatch_listen::listen_blocker(&project_id, api_url))
        .flatten();
    let listener = (opts.listen && opts.watch && blocker.is_none())
        .then(|| super::dispatch_listen::spawn_embedded(api_url));
    let runner = || {
        let mut r = runner_file.as_deref().map(runner_state).unwrap_or(Runner {
            state: "none".into(),
            note: None,
            last_error: None,
        });
        r.note = blocker.clone().or_else(|| {
            listener
                .as_ref()
                .and_then(|slot| slot.lock().ok().and_then(|s| s.clone()))
        });
        runner_view(
            r,
            &super::dispatch_listen::runner_scope(&project_id, api_url),
        )
    };

    if !opts.watch {
        let lists = fetch(&client, &project_id).await?;
        let now = now_ms();
        let snap = snapshot(
            project_name(&names),
            &names,
            &lists,
            runner(),
            opts.window_days,
            now,
        );
        if opts.json {
            println!("{}", serde_json::to_string_pretty(&snap)?);
        } else {
            let lines = if opts.compact {
                render_compact(&snap, now)
            } else {
                render(&snap, now)
            };
            for line in lines {
                println!("{line}");
            }
        }
        return Ok(());
    }

    let term = console::Term::stdout();
    let mut last: Option<Snapshot> = None;
    loop {
        let now = now_ms();
        match fetch(&client, &project_id).await {
            Ok(lists) => {
                let unknown = lists
                    .inbox
                    .iter()
                    .map(|d| &d.source_project_id)
                    .chain(lists.outbox.iter().map(|d| &d.target_project_id))
                    .any(|id| !names.contains_key(id));
                if unknown || names.is_empty() {
                    let fresh = project_names(&client).await;
                    if !fresh.is_empty() {
                        names = fresh;
                    }
                }
                last = Some(snapshot(
                    project_name(&names),
                    &names,
                    &lists,
                    runner(),
                    opts.window_days,
                    now,
                ));
            }
            Err(e) => {
                let snap = last.get_or_insert_with(|| {
                    snapshot(
                        project_name(&names),
                        &names,
                        &Lists {
                            inbox: vec![],
                            outbox: vec![],
                            inbox_done: vec![],
                            outbox_done: vec![],
                        },
                        runner(),
                        opts.window_days,
                        now,
                    )
                });
                snap.error = Some(format!(
                    "{} at {}Z",
                    error_text(e.as_ref()),
                    hhmm(&rfc3339(now))
                ));
            }
        }
        if let Some(snap) = last.as_ref() {
            if opts.json {
                println!("{}", serde_json::to_string(snap)?);
            } else {
                let lines = if opts.compact {
                    render_compact(snap, now)
                } else {
                    render(snap, now)
                };
                draw(&term, &lines)?;
            }
        }
        tokio::time::sleep(Duration::from_secs(opts.interval_secs)).await;
    }
}

// ---------------------------------------------------------------------------
// Workspace pane (`nexus run --dispatch-pane`)
// ---------------------------------------------------------------------------

/// KDL string literal.
fn kdl_string(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// The layout with a `dispatches` pane running `nexus dispatch status
/// --watch --compact` directly below the `claude` pane of the CLAUDE tab, at
/// 13% of the height like the hub workspace presets; the claude pane and its
/// new neighbour share the claude pane's place and size. A layout without a
/// claude pane gets a separate `dispatches` tab. `None` when the layout has
/// no closing brace.
fn layout_with_dispatch_pane(layout: &str, nexus: &str) -> Option<String> {
    let args = "args \"dispatch\" \"status\" \"--watch\" \"--compact\"";
    if let Some((at, end)) = claude_pane_span(layout) {
        let indent: String = layout[at..]
            .chars()
            .take_while(|c| *c == ' ' || *c == '\t')
            .collect();
        let block = &layout[at..end];
        let header_end = block.find('\n').unwrap_or(block.len());
        // The size moves to the wrapper, which takes the claude pane's place.
        let (header, size) = take_size(&block[..header_end]);
        let claude = format!("{header}{}", &block[header_end..]);
        let claude: String = claude.lines().map(|l| format!("    {l}\n")).collect();
        let size = size.map(|s| format!(" size={s}")).unwrap_or_default();
        let pane = format!(
            "{indent}pane split_direction=\"horizontal\"{size} {{\n{claude}{indent}    pane command={} name=\"dispatches\" size=\"13%\" {{\n{indent}        {args}\n{indent}    }}\n{indent}}}\n",
            kdl_string(nexus)
        );
        return Some(format!("{}{pane}{}", &layout[..at], &layout[end..]));
    }
    let end = layout.rfind('}')?;
    let tab = format!(
        "    tab name=\"dispatches\" {{\n        pane command={} name=\"dispatches\" {{\n            {args}\n        }}\n    }}\n",
        kdl_string(nexus)
    );
    let head = layout[..end].trim_end();
    Some(format!("{head}\n{tab}{}", &layout[end..]))
}

/// Byte span of the `pane ... name="claude"` node in the CLAUDE tab, from
/// the start of its line to the end of its last line (its child block
/// included).
fn claude_pane_span(layout: &str) -> Option<(usize, usize)> {
    let tab = layout.find("tab name=\"CLAUDE\"")?;
    let rel = layout[tab..].find("name=\"claude\"")?;
    let name_at = tab + rel;
    let at = layout[..name_at].rfind('\n').map_or(0, |i| i + 1);
    if !layout[at..name_at].trim_start().starts_with("pane") {
        return None;
    }
    let line_end = layout[name_at..]
        .find('\n')
        .map_or(layout.len(), |i| name_at + i);
    if !layout[at..line_end].trim_end().ends_with('{') {
        return Some((at, (line_end + 1).min(layout.len())));
    }
    // Match the child block's braces, skipping quoted strings.
    let (mut depth, mut quoted, mut escaped) = (0usize, false, false);
    for (i, c) in layout[at..].char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            '{' if !quoted => depth += 1,
            '}' if !quoted => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    let close = at + i + 1;
                    let end = layout[close..]
                        .find('\n')
                        .map_or(layout.len(), |j| close + j + 1);
                    return Some((at, end));
                }
            }
            _ => {}
        }
    }
    None
}

/// The node header without its `size="..."` property, and that value.
fn take_size(header: &str) -> (String, Option<String>) {
    let Some(at) = header.find(" size=\"") else {
        return (header.to_string(), None);
    };
    let value_start = at + " size=".len();
    let Some(close) = header[value_start + 1..].find('"') else {
        return (header.to_string(), None);
    };
    let value_end = value_start + 1 + close + 1;
    (
        format!("{}{}", &header[..at], &header[value_end..]),
        Some(header[value_start..value_end].to_string()),
    )
}

/// Whether a layout (or one tab of it) already runs the status view: a pane
/// named `dispatches`, or one running `dispatch status` (`zellij run` does
/// not keep the name in `dump-layout`).
fn has_dispatch_pane(layout: &str) -> bool {
    layout.contains("name=\"dispatches\"") || layout.contains("args \"dispatch\" \"status\"")
}

/// The focused tab of a `zellij action dump-layout`, or the whole dump
/// when no tab is marked.
fn focused_tab(dump: &str) -> &str {
    let Some(at) = dump.match_indices("tab ").map(|(i, _)| i).find(|&i| {
        dump[i..]
            .lines()
            .next()
            .is_some_and(|l| l.contains("focus=true"))
    }) else {
        return dump;
    };
    let rest = &dump[at + 4..];
    let end = rest
        .match_indices("\n    tab ")
        .map(|(i, _)| i)
        .next()
        .unwrap_or(rest.len());
    &dump[at..at + 4 + end]
}

/// `nexus run --dispatch-pane`: point the zellij `--layout` argument at a
/// copy of the layout with the Dispatch status pane. Returns a status line.
pub(crate) fn apply_dispatch_pane(
    workspace: &Path,
    leading_args: &mut [String],
) -> anyhow::Result<CheckResult> {
    let nexus = std::env::current_exe()?.display().to_string();
    let dir = config::Config::dir()?.join("state").join("layouts");
    extend_layout_arg(workspace, leading_args, &nexus, &dir)
}

/// Write the extended layout to `dir` and point `--layout` at it.
fn extend_layout_arg(
    workspace: &Path,
    leading_args: &mut [String],
    nexus: &str,
    dir: &Path,
) -> anyhow::Result<CheckResult> {
    let Some(i) = leading_args.iter().position(|a| a == "--layout") else {
        return Ok(CheckResult::Warn("no zellij layout to extend".into()));
    };
    let Some(layout_path) = leading_args.get(i + 1).cloned() else {
        return Ok(CheckResult::Warn("no zellij layout to extend".into()));
    };
    let layout = fs::read_to_string(&layout_path).ok();
    if layout.as_deref().is_some_and(has_dispatch_pane) {
        // The hub presets already carry the pane.
        return Ok(CheckResult::Info(
            "the layout already has a dispatches pane".into(),
        ));
    }
    let Some(kdl) = layout.and_then(|l| layout_with_dispatch_pane(&l, nexus)) else {
        return Ok(CheckResult::Warn(format!("could not extend {layout_path}")));
    };
    fs::create_dir_all(dir)?;
    let name = nexus_core::hash::sha256_hex(&workspace.display().to_string());
    let path = dir.join(format!("{}-dispatch.kdl", &name[..16]));
    fs::write(&path, kdl)?;
    leading_args[i + 1] = path.display().to_string();
    Ok(CheckResult::Pass(
        "nexus dispatch status --watch --compact".into(),
    ))
}

/// `nexus run` preflight for the runner in the dispatches pane: registered
/// and bound here, plus the Claude Code probe. Token and switches are
/// checked by the first heartbeat; the pane shows the result.
pub(crate) fn runner_check(workspace: &Path, api_url: &str) -> CheckResult {
    let Ok(project_id) = config::resolve_project_id(None, Some(workspace)) else {
        return CheckResult::Info("no linked project".into());
    };
    match super::dispatch_listen::listen_blocker(&project_id, api_url) {
        Some(reason) => CheckResult::Warn(format!("not listening: {reason}")),
        None => {
            let probe = super::dispatch_runner::probe_claude();
            CheckResult::Pass(format!(
                "listens in the dispatches pane (notify; claude_code inbox: {})",
                if probe.inbox.is_ok() {
                    "yes"
                } else {
                    "no, desktop notifications"
                }
            ))
        }
    }
}

/// `zellij run` arguments that open the status pane below the focused pane
/// of the current session.
fn session_pane_args(nexus: &str) -> Vec<String> {
    [
        "run",
        "--name",
        "dispatches",
        "--direction",
        "down",
        "--",
        nexus,
        "dispatch",
        "status",
        "--watch",
        "--compact",
    ]
    .map(str::to_string)
    .to_vec()
}

/// `nexus run --dispatch-pane` inside the project's running zellij session:
/// the layout no longer applies, so open the pane with `zellij run`.
pub(crate) fn open_pane_in_session() -> CheckResult {
    let nexus = match std::env::current_exe() {
        Ok(p) => p.display().to_string(),
        Err(e) => return CheckResult::Warn(format!("cannot locate nexus: {e}")),
    };
    let dump = std::process::Command::new("zellij")
        .args(["action", "dump-layout"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string());
    if dump
        .as_deref()
        .map(focused_tab)
        .is_some_and(has_dispatch_pane)
    {
        return CheckResult::Info("this tab already has a dispatches pane".into());
    }
    match std::process::Command::new("zellij")
        .args(session_pane_args(&nexus))
        .stdout(std::process::Stdio::null())
        .status()
    {
        Ok(s) if s.success() => {
            // Best effort: a failed resize leaves a half-height pane.
            for &args in SHRINK_PANE {
                let _ = std::process::Command::new("zellij")
                    .args(args)
                    .stdout(std::process::Stdio::null())
                    .stderr(std::process::Stdio::null())
                    .status();
            }
            CheckResult::Pass("opened below in the running session".into())
        }
        Ok(s) => CheckResult::Warn(format!("zellij run failed ({s})")),
        Err(e) => CheckResult::Warn(format!("zellij run failed: {e}")),
    }
}

/// `zellij run` splits the focused pane in half and focuses the new pane:
/// shrink it to about a fifth, then give the focus back to the pane above.
const SHRINK_PANE: &[&[&str]] = &[
    &["action", "resize", "decrease", "up"],
    &["action", "resize", "decrease", "up"],
    &["action", "resize", "decrease", "up"],
    &["action", "resize", "decrease", "up"],
    &["action", "resize", "decrease", "up"],
    &["action", "resize", "decrease", "up"],
    &["action", "move-focus", "up"],
];

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_793_000_000_000;
    const ME: &str = "00000000-0000-4000-8000-0000000000aa";
    const PEER: &str = "00000000-0000-4000-8000-0000000000bb";

    fn at(ms_ago: i64) -> String {
        rfc3339(NOW - ms_ago)
    }

    fn d(id: &str, source: &str, target: &str, status: &str) -> DispatchSummary {
        DispatchSummary {
            id: id.into(),
            source_project_id: source.into(),
            target_project_id: target.into(),
            title: format!("title {id}"),
            kind: "implementation_request".into(),
            priority: "normal".into(),
            status: status.into(),
            blocking: false,
            due_at: None,
            resolved_at: None,
            closed_at: None,
            created_at: at(3 * 86_400_000),
            updated_at: Some(at(3_600_000)),
        }
    }

    fn names() -> BTreeMap<String, String> {
        [
            (ME.to_string(), "NEXUS-CLI".to_string()),
            (PEER.to_string(), "NEXUS-APP".to_string()),
        ]
        .into()
    }

    fn snap(lists: &Lists) -> Snapshot {
        snapshot(
            ProjectRef {
                id: ME.into(),
                name: "NEXUS-CLI".into(),
            },
            &names(),
            lists,
            Runner {
                state: "none".into(),
                note: None,
                last_error: None,
            },
            7,
            NOW,
        )
    }

    fn sample() -> Lists {
        let mut done_recent = d("r1", PEER, ME, "resolved");
        done_recent.resolved_at = Some(at(86_400_000));
        let mut done_old = d("r2", PEER, ME, "closed");
        done_old.closed_at = Some(at(30 * 86_400_000));
        let mut out_closed = d("c1", ME, PEER, "closed");
        out_closed.closed_at = Some(at(2 * 86_400_000));
        let mut blocking = d("i3", PEER, ME, "in_progress");
        blocking.blocking = true;
        Lists {
            inbox: vec![
                d("i1", PEER, ME, "open"),
                d("i2", PEER, ME, "acknowledged"),
                blocking,
                d(
                    "i4",
                    "00000000-0000-4000-8000-0000000000cc",
                    ME,
                    "waiting_for_requester",
                ),
            ],
            outbox: vec![
                d("o1", ME, PEER, "waiting_for_requester"),
                d("o2", ME, PEER, "in_progress"),
                d("o3", ME, PEER, "open"),
            ],
            inbox_done: vec![done_recent, done_old],
            outbox_done: vec![out_closed],
        }
    }

    #[test]
    fn test_counters_follow_the_ownership_rules() {
        let s = snap(&sample());
        assert_eq!(s.incoming.open.value, 1);
        assert_eq!(s.incoming.waiting_on_us.value, 2); // open + acknowledged
        assert_eq!(s.incoming.in_progress.value, 1);
        assert_eq!(s.incoming.blocked.value, 0);
        assert_eq!(s.incoming.resolved.value, 1); // the old close is outside 7d
        assert_eq!(s.incoming.closed.value, 0);
        assert_eq!(s.outgoing.open.value, 1);
        assert_eq!(s.outgoing.waiting_on_us.value, 1); // waiting_for_requester
        assert_eq!(s.outgoing.waiting_on_peer.value, 2);
        assert_eq!(s.outgoing.closed.value, 1);
    }

    #[test]
    fn test_rows_put_our_turn_first_and_name_peers() {
        let s = snap(&sample());
        assert_eq!(s.dispatches.len(), 7);
        let ours: Vec<_> = s
            .dispatches
            .iter()
            .take_while(|r| r.waiting_on_us)
            .collect();
        assert_eq!(ours.len(), 3);
        assert!(s.dispatches.iter().any(|r| r.peer == "00000000")); // unknown peer: short id
        let app = s.peers.iter().find(|p| p.name == "NEXUS-APP").unwrap();
        assert_eq!((app.incoming, app.outgoing, app.waiting_on_us), (3, 3, 3));
        let o1 = s.dispatches.iter().find(|r| r.id == "o1").unwrap();
        assert_eq!(
            (o1.direction, o1.peer.as_str()),
            (Direction::Out, "NEXUS-APP")
        );
    }

    #[test]
    fn test_full_page_is_a_lower_bound() {
        let lists = Lists {
            inbox: (0..DISPATCH_LIST_LIMIT)
                .map(|n| d(&format!("i{n}"), PEER, ME, "open"))
                .collect(),
            outbox: vec![],
            inbox_done: vec![],
            outbox_done: vec![],
        };
        let s = snap(&lists);
        assert_eq!(s.incoming.open.show(), "50+");
        assert_eq!(s.outgoing.open.show(), "0");
    }

    #[test]
    fn test_json_shape_is_stable() {
        let v = serde_json::to_value(snap(&sample())).unwrap();
        assert_eq!(v["schema"], "nexus.dispatch-status.v1");
        assert_eq!(v["runner"]["state"], "none");
        assert_eq!(v["incoming"]["waiting_on_us"]["value"], 2);
        assert_eq!(v["outgoing"]["waiting_on_peer"]["truncated"], false);
        assert_eq!(v["dispatches"][0]["direction"], "in");
        assert!(v["dispatches"][0].get("type").is_some());
        assert!(v["error"].is_null());
    }

    #[test]
    fn test_compact_fits_a_narrow_pane() {
        let s = snap(&sample());
        let lines = render_compact(&s, NOW);
        // Everything before the title fits in 45 columns; draw() cuts the rest.
        for line in &lines {
            let plain = console::strip_ansi_codes(line);
            let head: String = plain.chars().take(45).collect();
            assert!(!head.contains('\n'));
        }
        let row = lines.iter().find(|l| l.contains("title o1")).unwrap();
        let plain = console::strip_ansi_codes(row);
        assert!(plain.find("title o1").unwrap() <= 28, "{plain}");
        assert!(console::strip_ansi_codes(&lines[1]).contains("2 ours"));
    }

    #[test]
    fn test_simulation_rows_are_recognised() {
        let mut lists = sample();
        lists.inbox[0].title = "[Simulation] Which Node version?".into();
        let s = snap(&lists);
        let sims: Vec<&Row> = s.dispatches.iter().filter(|r| is_simulation(r)).collect();
        assert_eq!(sims.len(), 1);
        assert_eq!(sims[0].title, "[Simulation] Which Node version?");
        let header = console::strip_ansi_codes(&render_compact(&s, NOW)[0]).to_string();
        assert!(header.contains("manager none"), "{header}");
    }

    #[test]
    fn test_full_view_marks_error_and_blocking() {
        let mut s = snap(&sample());
        s.error = Some("connection refused at 10:00:00Z".into());
        let text = render(&s, NOW)
            .iter()
            .map(|l| console::strip_ansi_codes(l).to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("refresh failed, showing last state"));
        assert!(text.contains("!in_progress"));
        assert!(text.contains("NEXUS-APP        3 in / 3 out, 3 waiting on us"));
    }

    #[test]
    fn test_rfc3339_round_trip() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        let t = "2026-11-06T12:39:21Z";
        assert_eq!(rfc3339(parse_rfc3339(t).unwrap()), t);
        assert_eq!(age(Some(&at(90_000)), NOW), "1m");
        assert_eq!(age(Some(&at(3 * 86_400_000)), NOW), "3d");
    }

    #[test]
    fn test_runner_view_names_the_scope() {
        use crate::cmd::dispatch_listen::RunnerScope;
        let r = |state: &str| Runner {
            state: state.into(),
            note: None,
            last_error: Some("API error: Insufficient permissions".into()),
        };
        let elsewhere = runner_view(r("running"), &RunnerScope::Unbound(vec!["Beta".into()]));
        assert_eq!(elsewhere.state, "elsewhere");
        assert_eq!(
            elsewhere.note.as_deref(),
            Some("serves Beta, not this project; run: nexus dispatch runner register")
        );
        // A stopped runner keeps its state and the caller's note.
        let stopped = runner_view(r("stopped"), &RunnerScope::Unbound(vec!["Beta".into()]));
        assert_eq!((stopped.state.as_str(), stopped.note), ("stopped", None));
        let bound = runner_view(r("running"), &RunnerScope::Bound);
        assert_eq!(
            bound.note.as_deref(),
            Some("last error: API error: Insufficient permissions")
        );
        let mut noted = r("running");
        noted.note = Some("listener stopped".into());
        assert_eq!(
            runner_view(noted, &RunnerScope::Bound).note.as_deref(),
            Some("listener stopped")
        );
    }

    #[test]
    fn test_runner_state_is_read_only_and_optional() {
        let dir = std::env::temp_dir().join(format!("nexus-runstate-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let file = dir.join("runner.json");
        assert_eq!(runner_state(&file).state, "none");
        fs::write(&file, r#"{"state":"listening","pid":1}"#).unwrap();
        assert_eq!(runner_state(&file).state, "listening");
        // A running state needs a live process.
        let me = std::process::id();
        fs::write(&file, format!(r#"{{"state":"running","pid":{me}}}"#)).unwrap();
        assert_eq!(runner_state(&file).state, "running");
        fs::write(&file, r#"{"state":"running","pid":0}"#).unwrap();
        assert_eq!(runner_state(&file).state, "stopped");
        fs::write(&file, "garbage").unwrap();
        assert_eq!(runner_state(&file).state, "unknown");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_dispatch_pane_goes_below_the_claude_pane() {
        let layout = "layout {\n    tab name=\"CLAUDE\" {\n        pane split_direction=\"vertical\" {\n            pane command=\"sh\" name=\"claude\" focus=true size=\"75%\" {\n                args \"-c\" \"claude; echo \\\"}\\\"\"\n            }\n            pane split_direction=\"horizontal\" {\n                pane name=\"terminal\"\n            }\n        }\n    }\n    tab name=\"GIT\" {\n        pane name=\"terminal\"\n    }\n}\n";
        let out = layout_with_dispatch_pane(layout, "/bin/nexus").unwrap();
        let expected = "            pane split_direction=\"horizontal\" size=\"75%\" {\n                pane command=\"sh\" name=\"claude\" focus=true {\n                    args \"-c\" \"claude; echo \\\"}\\\"\"\n                }\n                pane command=\"/bin/nexus\" name=\"dispatches\" size=\"13%\" {\n                    args \"dispatch\" \"status\" \"--watch\" \"--compact\"\n                }\n            }\n            pane split_direction=\"horizontal\" {\n                pane name=\"terminal\"";
        assert!(out.contains(expected), "{out}");
        assert_eq!(out.matches("name=\"dispatches\"").count(), 1);
        assert!(out.ends_with("    tab name=\"GIT\" {\n        pane name=\"terminal\"\n    }\n}\n"));
        // A one-line claude pane without a size.
        let one = "layout {\n    tab name=\"CLAUDE\" {\n        pane name=\"claude\"\n        pane name=\"terminal\"\n    }\n}\n";
        let out = layout_with_dispatch_pane(one, "/bin/nexus").unwrap();
        assert!(out.contains("        pane split_direction=\"horizontal\" {\n            pane name=\"claude\"\n            pane command=\"/bin/nexus\" name=\"dispatches\" size=\"13%\" {"), "{out}");
    }

    #[test]
    fn test_compact_totals_keep_the_lower_bound() {
        let mut lists = sample();
        lists.outbox_done = (0..DISPATCH_LIST_LIMIT)
            .map(|n| {
                let mut c = d(&format!("c{n}"), ME, PEER, "closed");
                c.closed_at = Some(at(86_400_000));
                c
            })
            .collect();
        let lines = render_compact(&snap(&lists), NOW);
        // Both counts come from the full outgoing list, so both are bounds.
        assert_eq!(
            console::strip_ansi_codes(&lines[3]),
            "7d  1+ resolved 50+ closed"
        );
    }

    #[test]
    fn test_compact_shows_error_and_empty_state() {
        let mut s = snap(&Lists {
            inbox: vec![],
            outbox: vec![],
            inbox_done: vec![],
            outbox_done: vec![],
        });
        s.error = Some("timeout at 10:00:00Z".into());
        let lines: Vec<String> = render_compact(&s, NOW)
            .iter()
            .map(|l| console::strip_ansi_codes(l).to_string())
            .collect();
        assert!(lines[4].starts_with("! refresh failed"));
        assert_eq!(lines.last().unwrap(), "no open Dispatches");
        let full: Vec<String> = render(&s, NOW)
            .iter()
            .map(|l| console::strip_ansi_codes(l).to_string())
            .collect();
        assert_eq!(full.last().unwrap(), "  No open Dispatches.");
    }

    #[test]
    fn test_self_addressed_dispatch_shows_both_ways() {
        let lists = Lists {
            inbox: vec![d("s1", ME, ME, "open")],
            outbox: vec![d("s1", ME, ME, "open")],
            inbox_done: vec![],
            outbox_done: vec![],
        };
        let s = snap(&lists);
        let dirs: Vec<_> = s.dispatches.iter().map(|r| r.direction).collect();
        assert_eq!(dirs.len(), 2);
        assert!(dirs.contains(&Direction::In) && dirs.contains(&Direction::Out));
        assert_eq!(s.peers.len(), 1);
        assert_eq!(s.peers[0].name, "NEXUS-CLI");
    }

    #[test]
    fn test_tones_highlight_what_needs_attention() {
        let paint = |c: usize, label: &str, tone: Tone| {
            counter_style(
                Count {
                    value: c,
                    truncated: false,
                },
                label,
                tone,
            )
            .force_styling(true)
            .to_string()
        };
        assert!(paint(2, "ours", Tone::Ours).contains("\x1b[33m"));
        assert!(paint(1, "blkd", Tone::Blocked).contains("\x1b[31m"));
        assert!(paint(1, "prog", Tone::Progress).contains("\x1b[34m"));
        assert!(paint(1, "open", Tone::Open).contains("\x1b[36m"));
        assert!(paint(1, "resolved", Tone::Resolved).contains("\x1b[32m"));
        // Zero is dimmed whatever its tone, also as a lower bound.
        let zero = paint(0, "ours", Tone::Ours);
        assert!(zero.contains("\x1b[2m") && !zero.contains("\x1b[33m"));
        let bound = counter_style(
            Count {
                value: 0,
                truncated: true,
            },
            "resolved",
            Tone::Resolved,
        )
        .force_styling(true)
        .to_string();
        assert!(bound.contains("\x1b[2m") && !bound.contains("\x1b[32m"));
        let full = counter_style(
            Count {
                value: 3,
                truncated: true,
            },
            "resolved",
            Tone::Resolved,
        )
        .force_styling(true)
        .to_string();
        assert!(full.contains("\x1b[32m"));

        let s = snap(&sample());
        let tone = |id: &str| status_tone(s.dispatches.iter().find(|r| r.id == id).unwrap());
        assert_eq!(tone("i1"), Tone::Ours); // open incoming: our turn
        assert_eq!(tone("i3"), Tone::Progress);
        assert_eq!(tone("o2"), Tone::Progress);
        assert_eq!(tone("o3"), Tone::Open); // open outgoing: the peer's turn
        assert_eq!(tone("o1"), Tone::Ours);
        let mut blocked = s.dispatches[0].clone();
        blocked.status = "blocked".into();
        blocked.waiting_on_us = true;
        assert_eq!(status_tone(&blocked), Tone::Blocked);
    }

    #[test]
    fn test_error_text_adds_new_causes_only() {
        #[derive(Debug)]
        struct E(&'static str, Option<Box<E>>);
        impl std::fmt::Display for E {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        impl std::error::Error for E {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                self.1.as_deref().map(|e| e as _)
            }
        }
        let e = E(
            "HTTP error: error sending request",
            Some(Box::new(E(
                "error sending request",
                Some(Box::new(E(
                    "connection closed before message completed",
                    None,
                ))),
            ))),
        );
        assert_eq!(
            error_text(&e),
            "HTTP error: error sending request: connection closed before message completed"
        );
    }

    #[test]
    fn test_fit_keeps_an_exact_width() {
        assert_eq!(fit("prog", 4), "prog");
        assert_eq!(fit("ack", 4), "ack ");
        assert_eq!(fit("progress", 4), "pro…");
    }

    #[test]
    fn test_status_abbreviations() {
        for (full, short) in [
            ("open", "open"),
            ("acknowledged", "ack"),
            ("in_progress", "prog"),
            ("waiting_for_requester", "wait"),
            ("needs_review", "revw"),
            ("blocked", "blkd"),
        ] {
            assert_eq!(status_short(full), short);
        }
    }

    #[test]
    fn test_extend_layout_arg_writes_a_copy() {
        let dir = std::env::temp_dir().join(format!("nexus-dispatch-pane-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("layout.kdl");
        fs::write(
            &src,
            "layout {\n    tab name=\"MAIN\" {\n        pane\n    }\n}\n",
        )
        .unwrap();
        let mut args = vec!["--layout".to_string(), src.display().to_string()];
        let out = dir.join("out");
        let result = extend_layout_arg(Path::new("/ws"), &mut args, "/bin/nexus", &out).unwrap();
        assert!(matches!(result, CheckResult::Pass(_)));
        assert!(
            args[1].ends_with("-dispatch.kdl") && args[1].starts_with(&out.display().to_string())
        );
        assert!(fs::read_to_string(&args[1])
            .unwrap()
            .contains("name=\"dispatches\""));
        assert!(!fs::read_to_string(&src).unwrap().contains("dispatches"));
        let mut none = vec!["--other".to_string()];
        assert!(matches!(
            extend_layout_arg(Path::new("/ws"), &mut none, "n", &out).unwrap(),
            CheckResult::Warn(_)
        ));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_session_pane_runs_the_compact_watch() {
        let args = session_pane_args("/bin/nexus");
        let sep = args.iter().position(|a| a == "--").unwrap();
        assert_eq!(
            &args[..5],
            ["run", "--name", "dispatches", "--direction", "down"]
        );
        assert_eq!(SHRINK_PANE.last().unwrap(), &["action", "move-focus", "up"]);
        assert_eq!(
            &args[sep + 1..],
            ["/bin/nexus", "dispatch", "status", "--watch", "--compact"]
        );
    }

    #[test]
    fn test_layout_with_a_dispatches_pane_is_left_alone() {
        // The hub preset (standard-terminal v2): the pane is already there.
        let preset = "layout {\n    tab name=\"CLAUDE\" focus=true {\n        pane split_direction=\"vertical\" {\n            pane split_direction=\"horizontal\" size=\"75%\" {\n                pane command=\"sh\" name=\"claude\" focus=true\n                pane command=\"nexus\" name=\"dispatches\" size=\"13%\" {\n                    args \"dispatch\" \"status\" \"--watch\" \"--compact\"\n                }\n            }\n            pane name=\"terminal\"\n        }\n    }\n}\n";
        let dir = std::env::temp_dir().join(format!("nexus-preset-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let layout = dir.join("preset.kdl");
        fs::write(&layout, preset).unwrap();
        let mut args = vec!["--layout".to_string(), layout.display().to_string()];
        let r = extend_layout_arg(&dir, &mut args, "/bin/nexus", &dir.join("out")).unwrap();
        assert!(matches!(r, CheckResult::Info(_)));
        assert_eq!(args[1], layout.display().to_string(), "layout unchanged");
        // Without one, the pane is still added once.
        let plain =
            "layout {\n    tab name=\"CLAUDE\" {\n        pane name=\"terminal\"\n    }\n}\n";
        fs::write(&layout, plain).unwrap();
        let mut args = vec!["--layout".to_string(), layout.display().to_string()];
        let r = extend_layout_arg(&dir, &mut args, "/bin/nexus", &dir.join("out")).unwrap();
        assert!(matches!(r, CheckResult::Pass(_)));
        let out = fs::read_to_string(&args[1]).unwrap();
        assert_eq!(out.matches("args \"dispatch\" \"status\"").count(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_focused_tab_is_checked_for_a_pane() {
        let dump = "layout {\n    tab name=\"CLAUDE\" focus=true {\n        pane command=\"/bin/nexus\" size=\"20%\" {\n            args \"dispatch\" \"status\" \"--watch\" \"--compact\"\n        }\n    }\n    tab name=\"GIT\" {\n        pane command=\"lazygit\"\n    }\n}\n";
        assert!(has_dispatch_pane(focused_tab(dump)));
        let other = dump
            .replace("CLAUDE\" focus=true", "CLAUDE\"")
            .replace("GIT\"", "GIT\" focus=true");
        assert!(!has_dispatch_pane(focused_tab(&other)));
    }

    #[test]
    fn test_dispatch_pane_falls_back_to_a_tab() {
        let layout = "layout {\n    tab name=\"MAIN\" {\n        pane\n    }\n}\n";
        let out = layout_with_dispatch_pane(layout, "/bin/nexus").unwrap();
        assert!(out.contains("tab name=\"dispatches\""));
        assert!(out.trim_end().ends_with('}'));
        assert_eq!(layout_with_dispatch_pane("no braces", "n"), None);
    }
}
