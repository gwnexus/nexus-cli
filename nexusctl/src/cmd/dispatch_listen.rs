//! `nexus dispatch listen`: the Dispatch runner loop in notify mode.
//!
//! It polls the hub event feed with the runner token, turns the events that
//! concern a bound project into short notices, appends them to the notice
//! queue and delivers them into running sessions (or as a desktop
//! notification). It starts no agent, claims nothing and writes no status.
//!
//! Files under `~/.config/nexus/state/dispatch/`:
//! - `runner.lock`: exclusive lock, one runner per machine;
//! - `runner.json`: state, written atomically (read by `dispatch status`);
//! - `notices/<project>.jsonl`: the notice queue, this loop is its only
//!   writer;
//! - `notices/<project>.delivered.runner.jsonl`: which session got which
//!   notice from this loop;
//! - `sessions/<session-id>.json`: running sessions, written by the
//!   runtime plugin (mode 0600, read only here).
//!
//! The cursor advances only after the notices of a batch are recorded, and a
//! notice id is derived from its event, so a restart or a repeated event
//! never queues a notice twice.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use console::style;
use nexus_core::api::{
    DispatchEvent, DispatchGetResponse, NexusClient, RunnerBinding, RunnerHeartbeatResponse,
};
use serde::{Deserialize, Serialize};

use super::dispatch_runner::{
    self, load_runner_file, probe_claude, repo_path_hash, runner_client, runner_file_path,
    state_dir, write_private, RunnerFile,
};
use super::dispatch_status::rfc3339_now;
use super::observer::now_ms;

/// Events read per call; more pages follow while `has_more`.
const EVENT_PAGE: u32 = 200;
/// Pages read per poll, so a long-stopped runner catches up in steps.
const MAX_PAGES: usize = 5;
/// Notices older than this are left to the session-start catch-up.
const DELIVERY_WINDOW_MS: i64 = 24 * 3_600_000;

// ---------------------------------------------------------------------------
// Notices
// ---------------------------------------------------------------------------

/// Why a notice was raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum NoticeKind {
    /// A new incoming Dispatch.
    Incoming,
    /// A reply or clarification on a Dispatch we requested.
    Reply,
    /// Our outgoing Dispatch was resolved.
    Resolved,
    /// Our outgoing Dispatch moved to waiting_for_requester or needs_review.
    NeedsReply,
}

/// One line of `notices/<project>.jsonl`. The text never holds the body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Notice {
    pub id: String,
    pub project_id: String,
    pub dispatch_id: String,
    pub event_id: i64,
    pub kind: NoticeKind,
    pub mode: String,
    pub text: String,
    pub created_at: String,
}

/// What an event means for a bound project, before the Dispatch is read.
/// `None` when it raises no notice.
pub(crate) fn trigger(e: &DispatchEvent, project: &str) -> Option<NoticeKind> {
    let incoming = e.target_project_id.as_deref() == Some(project);
    let outgoing = e.source_project_id.as_deref() == Some(project);
    match e.event_type.as_str() {
        "created" if incoming && !outgoing => Some(NoticeKind::Incoming),
        "rerouted" if incoming => Some(NoticeKind::Incoming),
        "status_changed" if outgoing => match e.status_to.as_deref() {
            Some("waiting_for_requester" | "needs_review") => Some(NoticeKind::NeedsReply),
            Some("resolved") => Some(NoticeKind::Resolved),
            _ => None,
        },
        "message_added" if outgoing => match e.message_type.as_deref() {
            Some("reply" | "clarification" | "blocker") => Some(NoticeKind::Reply),
            _ => None,
        },
        _ => None,
    }
}

/// A title safe for a one-line notice: no line breaks or quotes, at most
/// 100 characters.
fn clean_title(title: &str) -> String {
    let flat: String = title
        .chars()
        .map(|c| if c.is_control() || c == '"' { ' ' } else { c })
        .collect();
    let flat = flat.split_whitespace().collect::<Vec<_>>().join(" ");
    console::truncate_str(&flat, 100, "…").to_string()
}

/// The notice text: Dispatch id, title and peer, never the body.
pub(crate) fn notice_text(
    kind: NoticeKind,
    d: Option<&DispatchGetResponse>,
    dispatch_id: &str,
    peer: &str,
    status_to: Option<&str>,
    message_type: Option<&str>,
) -> String {
    let short: String = dispatch_id.chars().take(8).collect();
    let title = d
        .map(|d| format!(" \"{}\"", clean_title(&d.dispatch.title)))
        .unwrap_or_default();
    match kind {
        NoticeKind::Incoming => {
            let meta = d
                .map(|d| format!(" ({}, {})", d.dispatch.kind, d.dispatch.priority))
                .unwrap_or_default();
            format!("Nexus: new Dispatch {short}{title} from {peer}{meta}. Run /nexus-dispatch-sweep.")
        }
        NoticeKind::NeedsReply => format!(
            "Nexus: Dispatch {short}{title} from {peer} needs your reply ({}). Run /nexus-dispatch-sweep.",
            status_to.unwrap_or("waiting_for_requester")
        ),
        NoticeKind::Reply => format!(
            "Nexus: {peer} added a {} on Dispatch {short}{title}. Run /nexus-dispatch-sweep.",
            message_type.unwrap_or("reply")
        ),
        NoticeKind::Resolved => format!(
            "Nexus: Dispatch {short}{title} was resolved by {peer}. Review and close it with /nexus-dispatch-sweep."
        ),
    }
}

/// The bound projects an event concerns: from the combined feed, or by
/// looking at both sides.
fn concerned(e: &DispatchEvent, bound: &BTreeSet<String>) -> Vec<String> {
    if !e.bound_project_ids.is_empty() {
        return e
            .bound_project_ids
            .iter()
            .filter(|p| bound.contains(*p))
            .cloned()
            .collect();
    }
    [
        e.source_project_id.as_ref(),
        e.target_project_id.as_ref(),
        e.previous_target_project_id.as_ref(),
    ]
    .into_iter()
    .flatten()
    .filter(|p| bound.contains(*p))
    .cloned()
    .collect::<BTreeSet<_>>()
    .into_iter()
    .collect()
}

/// A candidate before the Dispatch is read: within one batch, the strongest
/// trigger per project and Dispatch wins (a reply that also moves the
/// status raises one notice, not two).
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub project_id: String,
    pub kind: NoticeKind,
    pub event: DispatchEvent,
}

pub(crate) fn candidates(events: &[DispatchEvent], active: &BTreeSet<String>) -> Vec<Candidate> {
    let mut best: BTreeMap<(String, String), Candidate> = BTreeMap::new();
    for e in events {
        for p in concerned(e, active) {
            let Some(kind) = trigger(e, &p) else {
                continue;
            };
            let key = (p.clone(), e.dispatch_id.clone());
            let replace = best.get(&key).is_none_or(|c| kind >= c.kind);
            if replace {
                best.insert(
                    key,
                    Candidate {
                        project_id: p,
                        kind,
                        event: e.clone(),
                    },
                );
            }
        }
    }
    let mut out: Vec<Candidate> = best.into_values().collect();
    out.sort_by_key(|c| c.event.id);
    out
}

pub(crate) fn notice_id(project_id: &str, event_id: i64) -> String {
    format!(
        "{}-{event_id}",
        project_id.chars().take(8).collect::<String>()
    )
}

/// Build the notice; `None` when the message came from the project itself
/// (its own reply on its own request).
pub(crate) fn build_notice(
    c: &Candidate,
    d: Option<&DispatchGetResponse>,
    mode: &str,
    name_of: &dyn Fn(&str) -> String,
) -> Option<Notice> {
    if c.kind == NoticeKind::Reply {
        let author = d.and_then(|d| {
            d.messages
                .iter()
                .find(|m| Some(&m.id) == c.event.message_id.as_ref())
                .and_then(|m| m.author_project_id.clone())
        });
        if author.as_deref() == Some(c.project_id.as_str()) {
            return None;
        }
    }
    let e = &c.event;
    let peer_id = if e.target_project_id.as_deref() == Some(&c.project_id) {
        e.source_project_id.as_deref()
    } else {
        e.target_project_id.as_deref()
    };
    let peer = peer_id.map(name_of).unwrap_or_else(|| "?".into());
    Some(Notice {
        id: notice_id(&c.project_id, e.id),
        project_id: c.project_id.clone(),
        dispatch_id: e.dispatch_id.clone(),
        event_id: e.id,
        kind: c.kind,
        mode: mode.to_string(),
        text: notice_text(
            c.kind,
            d,
            &e.dispatch_id,
            &peer,
            e.status_to.as_deref(),
            e.message_type.as_deref(),
        ),
        created_at: rfc3339_now(),
    })
}

// ---------------------------------------------------------------------------
// Queue
// ---------------------------------------------------------------------------

/// The notice queue and its delivery records under `<state>/notices`.
pub(crate) struct Queue {
    dir: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Delivery {
    pub id: String,
    /// The session, or `desktop` for a desktop notification.
    pub session_id: String,
    pub via: String,
    pub at: String,
}

fn read_jsonl<T: for<'de> Deserialize<'de>>(path: &Path) -> Vec<T> {
    fs::read_to_string(path)
        .map(|body| {
            body.lines()
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut opts = fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path)?;
    f.write_all(format!("{line}\n").as_bytes())?;
    f.sync_data()
}

impl Queue {
    pub fn new(dir: PathBuf) -> Self {
        Queue { dir }
    }

    fn notices_path(&self, project: &str) -> PathBuf {
        self.dir.join(format!("{project}.jsonl"))
    }

    fn deliveries_path(&self, project: &str) -> PathBuf {
        self.dir.join(format!("{project}.delivered.runner.jsonl"))
    }

    pub fn notices(&self, project: &str) -> Vec<Notice> {
        read_jsonl(&self.notices_path(project))
    }

    /// Append unless a notice with this id is already queued. Returns
    /// whether it was appended.
    pub fn record(&self, n: &Notice) -> std::io::Result<bool> {
        if self.notices(&n.project_id).iter().any(|q| q.id == n.id) {
            return Ok(false);
        }
        append_line(
            &self.notices_path(&n.project_id),
            &serde_json::to_string(n).map_err(std::io::Error::other)?,
        )?;
        Ok(true)
    }

    pub fn deliveries(&self, project: &str) -> Vec<Delivery> {
        read_jsonl(&self.deliveries_path(project))
    }

    pub fn mark(&self, project: &str, d: &Delivery) -> std::io::Result<()> {
        append_line(
            &self.deliveries_path(project),
            &serde_json::to_string(d).map_err(std::io::Error::other)?,
        )
    }
}

// ---------------------------------------------------------------------------
// Delivery (Claude Code adapter, shaped after ACP v1)
// ---------------------------------------------------------------------------

/// A running session as the runtime plugin announces it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct SessionFile {
    pub session_id: String,
    pub cwd: String,
    pub socket: String,
    /// Messaging token of that session; used for the auth line only.
    pub token: String,
}

/// The session's own setting for inbound peer messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Inbound {
    /// Not set: Claude Code applies its own default.
    Unset,
    Accept,
    Hold,
    Refuse,
}

fn inbound_value(path: &Path) -> Inbound {
    let Ok(body) = fs::read_to_string(path) else {
        return Inbound::Unset;
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) else {
        return Inbound::Unset;
    };
    match v.get("crossSessionInbound").and_then(|s| s.as_str()) {
        Some("accept") => Inbound::Accept,
        Some("hold") => Inbound::Hold,
        Some("refuse") => Inbound::Refuse,
        // An unknown value makes Claude Code hold messages.
        Some(_) => Inbound::Hold,
        None => Inbound::Unset,
    }
}

/// Settings files that can set `crossSessionInbound` for a session in
/// `root`: managed, user, project, local.
pub(crate) fn inbound_sources(root: &Path, home: &Path) -> Vec<PathBuf> {
    let managed = if cfg!(target_os = "macos") {
        PathBuf::from("/Library/Application Support/ClaudeCode/managed-settings.json")
    } else {
        PathBuf::from("/etc/claude-code/managed-settings.json")
    };
    vec![
        managed,
        home.join(".claude").join("settings.json"),
        root.join(".claude").join("settings.json"),
        root.join(".claude").join("settings.local.json"),
    ]
}

/// The strictest explicit value wins: a session that holds or refuses in
/// any layer gets nothing pushed.
pub(crate) fn inbound_policy(sources: &[PathBuf]) -> Inbound {
    let explicit: Vec<Inbound> = sources
        .iter()
        .map(|p| inbound_value(p))
        .filter(|v| *v != Inbound::Unset)
        .collect();
    explicit.into_iter().max().unwrap_or(Inbound::Unset)
}

/// The two frames of one push: the auth line, then the notice as a user
/// message.
pub(crate) fn inbox_frames(token: &str, text: &str) -> String {
    format!(
        "{}\n{}\n",
        serde_json::json!({ "type": "auth", "token": token }),
        serde_json::json!({ "type": "user", "message": { "role": "user", "content": text } })
    )
}

/// The delivery interface (shaped after ACP v1). Notify mode implements only
/// `inspect_capabilities` and `notify_session`.
#[allow(dead_code)]
pub(crate) trait RuntimeAdapter {
    fn inspect_capabilities(&self) -> Vec<String>;
    async fn notify_session(&self, session: &SessionFile, text: &str) -> Result<(), String>;
    fn create_session(&self) -> Result<(), String> {
        Err("create_session is not available in notify mode".into())
    }
    fn resume_session(&self) -> Result<(), String> {
        Err("resume_session is not available in notify mode".into())
    }
    fn submit(&self) -> Result<(), String> {
        Err("submit is not available in notify mode".into())
    }
    fn observe(&self) -> Result<(), String> {
        Err("observe is not available in notify mode".into())
    }
    fn cancel(&self) -> Result<(), String> {
        Err("cancel is not available in notify mode".into())
    }
}

/// Claude Code: notices go into the session's cross-session inbox socket.
pub(crate) struct ClaudeAdapter {
    pub inbox: bool,
}

impl RuntimeAdapter for ClaudeAdapter {
    fn inspect_capabilities(&self) -> Vec<String> {
        if self.inbox {
            vec![dispatch_runner::CROSS_SESSION_INBOX.into()]
        } else {
            vec![]
        }
    }

    async fn notify_session(&self, session: &SessionFile, text: &str) -> Result<(), String> {
        if !self.inbox {
            return Err("cross-session inbox not available".into());
        }
        #[cfg(unix)]
        {
            use tokio::io::AsyncWriteExt;
            let send = async {
                let mut sock = tokio::net::UnixStream::connect(&session.socket).await?;
                sock.write_all(inbox_frames(&session.token, text).as_bytes())
                    .await?;
                sock.shutdown().await
            };
            match tokio::time::timeout(Duration::from_secs(3), send).await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(format!("inbox socket: {}", e.kind())),
                Err(_) => Err("inbox socket: timed out".into()),
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (session, text);
            Err("cross-session inbox needs a Unix socket".into())
        }
    }
}

/// Sessions announced under `<state>/sessions` whose socket still exists.
pub(crate) fn sessions(dir: &Path) -> Vec<SessionFile> {
    let Ok(entries) = fs::read_dir(dir) else {
        return vec![];
    };
    entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| {
            serde_json::from_str::<SessionFile>(&fs::read_to_string(e.path()).ok()?).ok()
        })
        .filter(|s| Path::new(&s.socket).exists())
        .collect()
}

/// The repository a session runs in, and its hash.
fn session_root(s: &SessionFile) -> PathBuf {
    let cwd = PathBuf::from(&s.cwd);
    super::shadow_guard::repo_root(&cwd).unwrap_or(cwd)
}

/// Desktop notification, best effort.
fn desktop_notify(text: &str) -> bool {
    let quoted = text.replace('\\', "\\\\").replace('"', "\\\"");
    let status = if cfg!(target_os = "macos") {
        std::process::Command::new("osascript")
            .args([
                "-e",
                &format!("display notification \"{quoted}\" with title \"Nexus\""),
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
    } else {
        std::process::Command::new("notify-send")
            .args(["Nexus", text])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
    };
    status.is_ok_and(|s| s.success())
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct ProjectState {
    pub mode_effective: Option<String>,
    pub blocked_by: Option<String>,
    pub queued: u64,
    pub delivered: u64,
    pub failed: u64,
}

/// `runner.json`; `state` is what `dispatch status` shows. No token.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct RunnerState {
    pub state: String,
    pub pid: u32,
    pub runner_id: String,
    pub cli_version: String,
    /// Last event id whose notices are recorded.
    pub cursor: Option<i64>,
    pub updated_at: String,
    pub last_poll_at: Option<String>,
    pub last_heartbeat_at: Option<String>,
    pub last_error: Option<String>,
    pub claude_inbox: bool,
    pub projects: BTreeMap<String, ProjectState>,
}

fn load_state(path: &Path) -> RunnerState {
    fs::read_to_string(path)
        .ok()
        .and_then(|b| serde_json::from_str(&b).ok())
        .unwrap_or_default()
}

fn save_state(path: &Path, s: &mut RunnerState) {
    s.updated_at = rfc3339_now();
    if let Ok(body) = serde_json::to_string_pretty(s) {
        let _ = write_private(path, &(body + "\n"));
    }
}

/// Exclusive lock for the process lifetime: one runner per machine.
pub(crate) struct RunnerLock {
    _file: fs::File,
}

pub(crate) fn lock(path: &Path) -> anyhow::Result<RunnerLock> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // A child process forked elsewhere in this process can hold a copy
        // of a just-released lock until it execs, so retry briefly.
        let mut tries = 0;
        loop {
            // SAFETY: the descriptor belongs to `file`, which outlives the lock.
            let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
            if rc == 0 {
                break;
            }
            tries += 1;
            if tries >= 20 {
                anyhow::bail!("another runner is already listening on this machine");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    Ok(RunnerLock { _file: file })
}

// ---------------------------------------------------------------------------
// Loop
// ---------------------------------------------------------------------------

pub struct ListenOptions {
    pub interval_secs: u64,
    pub heartbeat_secs: u64,
    /// One poll and delivery round, then exit.
    pub once: bool,
}

/// Counters since the last heartbeat, per project.
#[derive(Default)]
struct Pending {
    delivered: BTreeMap<String, u64>,
    failed: BTreeMap<String, u64>,
    errors: BTreeMap<String, Option<String>>,
}

fn active_projects(bindings: &[RunnerBinding]) -> BTreeSet<String> {
    bindings
        .iter()
        .filter(|b| b.mode_effective.as_deref().is_some_and(|m| m != "off"))
        .map(|b| b.project_id.clone())
        .collect()
}

/// Set when the loop runs inside the status pane, which draws the screen.
static QUIET: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn log(line: &str) {
    if QUIET.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let t = rfc3339_now();
    println!("{} {line}", style(&t[11..16]).dim());
}

/// The binary's identity, to notice an upgrade and re-execute.
fn exe_stamp() -> Option<(PathBuf, std::time::SystemTime, u64)> {
    let exe = std::env::current_exe().ok()?;
    let meta = fs::metadata(&exe).ok()?;
    Some((exe, meta.modified().ok()?, meta.len()))
}

/// `nexus dispatch listen`.
pub async fn listen(opts: ListenOptions) -> anyhow::Result<()> {
    let env = ListenEnv {
        runner_file: runner_file_path()?,
        state_dir: state_dir()?,
        home: dirs_home(),
        claude_inbox: probe_claude().inbox.is_ok(),
        desktop: true,
        embedded: false,
    };
    run_loop(opts, env).await
}

/// Why the runner cannot listen for `project_id` here, checked before the
/// loop starts (registered, project bound).
pub(crate) fn listen_blocker(project_id: &str) -> Option<String> {
    let path = runner_file_path().ok()?;
    match load_runner_file(&path) {
        Ok(None) => Some("not registered (nexus dispatch runner register)".into()),
        Err(_) => Some("runner file unreadable (nexus dispatch runner register)".into()),
        Ok(Some(f)) if !f.projects.iter().any(|p| p.project_id == project_id) => {
            Some("this project is not bound (nexus dispatch runner register)".into())
        }
        Ok(Some(_)) => None,
    }
}

/// Start the loop inside the status pane. The returned slot holds the
/// reason when it stopped (or could not start); another process already
/// listening is not a failure.
pub(crate) fn spawn_embedded() -> std::sync::Arc<std::sync::Mutex<Option<String>>> {
    let slot = std::sync::Arc::new(std::sync::Mutex::new(None));
    let out = slot.clone();
    QUIET.store(true, std::sync::atomic::Ordering::Relaxed);
    tokio::spawn(async move {
        let env = match (runner_file_path(), state_dir()) {
            (Ok(runner_file), Ok(state_dir)) => ListenEnv {
                runner_file,
                state_dir,
                home: dirs_home(),
                claude_inbox: probe_claude().inbox.is_ok(),
                desktop: true,
                embedded: true,
            },
            _ => return,
        };
        let opts = ListenOptions {
            interval_secs: 30,
            heartbeat_secs: 60,
            once: false,
        };
        if let Err(e) = run_loop(opts, env).await {
            let msg = e.to_string();
            if !msg.contains("already listening") {
                *out.lock().unwrap() = Some(msg);
            }
        }
    });
    slot
}

/// Where the loop reads and writes, and what it may use; tests point it at
/// a temp directory without desktop notifications.
pub(crate) struct ListenEnv {
    pub runner_file: PathBuf,
    pub state_dir: PathBuf,
    pub home: PathBuf,
    pub claude_inbox: bool,
    pub desktop: bool,
    /// Inside `dispatch status --listen`: no log lines, and Ctrl-C is left
    /// to the process.
    pub embedded: bool,
}

pub(crate) async fn run_loop(opts: ListenOptions, env: ListenEnv) -> anyhow::Result<()> {
    let file = load_runner_file(&env.runner_file)?.ok_or_else(|| {
        anyhow::anyhow!(
            "No runner registered on this machine. Run 'nexus dispatch runner register'."
        )
    })?;
    let base = env.state_dir.clone();
    let _lock = lock(&base.join("runner.lock"))?;
    let state_path = base.join("runner.json");
    let queue = Queue::new(base.join("notices"));
    let sessions_dir = base.join("sessions");
    let client = runner_client(&file)?;
    let adapter = ClaudeAdapter {
        inbox: env.claude_inbox,
    };
    let home = env.home.clone();

    let mut state = load_state(&state_path);
    if state.runner_id != file.runner_id {
        // Another registration: its cursor does not apply.
        state = RunnerState::default();
    }
    state.state = "starting".into();
    state.pid = std::process::id();
    state.runner_id = file.runner_id.clone();
    state.cli_version = env!("CARGO_PKG_VERSION").into();
    state.claude_inbox = adapter.inbox;
    save_state(&state_path, &mut state);
    log(&format!(
        "runner {} listening for {} project(s) on {} (claude_code inbox: {})",
        file.name,
        file.projects.len(),
        file.api_url,
        if adapter.inbox {
            "yes"
        } else {
            "no, desktop notifications"
        }
    ));

    let stamp = exe_stamp();
    let mut pending = Pending::default();
    let mut bindings: Vec<RunnerBinding> = vec![];
    let mut next_beat = Instant::now();
    let embedded = env.embedded;
    let mut stop: std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<()>> + Send>> =
        if embedded {
            Box::pin(std::future::pending())
        } else {
            Box::pin(tokio::signal::ctrl_c())
        };

    loop {
        if Instant::now() >= next_beat {
            match heartbeat(&client, &state, &mut pending, &bindings).await {
                Ok(hb) => {
                    bindings = hb.projects.clone();
                    state.last_heartbeat_at = Some(rfc3339_now());
                    // Fix the start only once the runner may read: a cursor
                    // taken while blocked could predate the whole history.
                    let hub_cursor = hb.projects.iter().any(|b| b.cursor.is_some());
                    let unset = state.cursor.is_none() || (state.cursor == Some(0) && !hub_cursor);
                    if unset && !active_projects(&bindings).is_empty() {
                        state.cursor = Some(start_cursor(&hb));
                    }
                    for b in &bindings {
                        let p = state.projects.entry(b.project_id.clone()).or_default();
                        p.mode_effective = b.mode_effective.clone();
                        p.blocked_by = b.blocked_by.clone();
                    }
                }
                Err(e) => {
                    if matches!(e, nexus_core::Error::Unauthorized(_)) {
                        state.state = "revoked".into();
                        state.last_error = Some("the runner token was rejected".into());
                        save_state(&state_path, &mut state);
                        anyhow::bail!(
                            "the hub rejected the runner token (revoked or expired); run 'nexus dispatch runner register'"
                        );
                    }
                    state.last_error = Some(super::dispatch_status::error_text(&e));
                }
            }
            next_beat = Instant::now() + Duration::from_secs(opts.heartbeat_secs);
        }

        let active = active_projects(&bindings);
        if bindings.is_empty() {
            state.state = "error".into();
        } else if active.is_empty() {
            let reason = blocked_reason(&file, &bindings);
            if state.state != "blocked" || state.last_error.as_deref() != Some(reason.as_str()) {
                log(&format!("{} {reason}", style("idle").yellow()));
            }
            state.state = "blocked".into();
            state.last_error = Some(reason);
        } else {
            match poll(&client, &file, &queue, &mut state, &active, &bindings).await {
                Ok(n) => {
                    state.state = "running".into();
                    if n > 0 {
                        state.last_error = None;
                    }
                }
                Err(e) => {
                    state.state = "running".into();
                    state.last_error = Some(e);
                }
            }
            deliver(
                &file,
                &queue,
                &adapter,
                &sessions_dir,
                &home,
                &active,
                env.desktop,
                &mut state,
                &mut pending,
            )
            .await;
        }
        state.last_poll_at = Some(rfc3339_now());
        save_state(&state_path, &mut state);

        if opts.once {
            // Report what this round delivered before leaving.
            let _ = heartbeat(&client, &state, &mut pending, &bindings).await;
            return Ok(());
        }
        tokio::select! {
            _ = &mut stop => {
                state.state = "stopped".into();
                save_state(&state_path, &mut state);
                log("stopped");
                return Ok(());
            }
            _ = tokio::time::sleep(Duration::from_secs(opts.interval_secs)) => {}
        }
        if let Some((exe, modified, len)) = &stamp {
            if exe_stamp().is_some_and(|(_, m, l)| (&m, &l) != (modified, len)) {
                state.state = "restarting".into();
                save_state(&state_path, &mut state);
                log("binary changed, restarting");
                drop(_lock);
                return reexec(exe);
            }
        }
    }
}

fn dirs_home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(unix)]
fn reexec(exe: &Path) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .exec();
    Err(anyhow::anyhow!("re-exec failed: {err}"))
}

#[cfg(not(unix))]
fn reexec(_exe: &Path) -> anyhow::Result<()> {
    anyhow::bail!("the binary changed; start the runner again")
}

/// Why every binding is idle, for the log and the state file.
fn blocked_reason(file: &RunnerFile, bindings: &[RunnerBinding]) -> String {
    bindings
        .iter()
        .map(|b| {
            format!(
                "{} blocked by {}",
                file.name_of(&b.project_id),
                b.blocked_by.as_deref().unwrap_or("mode off")
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Where a fresh runner starts: the hub's cursor when it has one (a restart
/// after a lost state), otherwise the newest event (no backlog flood).
fn start_cursor(hb: &RunnerHeartbeatResponse) -> i64 {
    let hub: Vec<i64> = hb.projects.iter().filter_map(|b| b.cursor).collect();
    if !hub.is_empty() {
        return hub.into_iter().min().unwrap_or(0);
    }
    hb.projects
        .iter()
        .filter_map(|b| b.latest_event_id)
        .max()
        .unwrap_or(0)
}

/// The heartbeat report: cursor per binding (the global cursor covers every
/// binding), counts since the last beat and the last error.
pub(crate) fn heartbeat_report(
    cursor: Option<i64>,
    bindings: &[String],
    delivered: &BTreeMap<String, u64>,
    failed: &BTreeMap<String, u64>,
    errors: &BTreeMap<String, Option<String>>,
) -> serde_json::Value {
    let mut body = serde_json::json!({ "cli_version": env!("CARGO_PKG_VERSION") });
    if let Some(c) = cursor {
        body["cursors"] = bindings
            .iter()
            .map(|p| (p.clone(), serde_json::json!(c)))
            .collect();
    }
    let counts = |m: &BTreeMap<String, u64>| -> serde_json::Value {
        m.iter()
            .filter(|(p, n)| **n > 0 && bindings.contains(p))
            .map(|(p, n)| (p.clone(), serde_json::json!(n.min(&100_000))))
            .collect()
    };
    if delivered.values().any(|n| *n > 0) {
        body["delivered"] = counts(delivered);
    }
    if failed.values().any(|n| *n > 0) {
        body["failed"] = counts(failed);
    }
    if !errors.is_empty() {
        body["errors"] = errors
            .iter()
            .filter(|(p, _)| bindings.contains(p))
            .map(|(p, e)| (p.clone(), serde_json::json!(e)))
            .collect();
    }
    body
}

async fn heartbeat(
    client: &NexusClient,
    state: &RunnerState,
    pending: &mut Pending,
    bindings: &[RunnerBinding],
) -> Result<RunnerHeartbeatResponse, nexus_core::Error> {
    let bound: Vec<String> = bindings.iter().map(|b| b.project_id.clone()).collect();
    let body = heartbeat_report(
        // Only report a cursor for bindings the hub already confirmed.
        state.cursor.filter(|_| !bound.is_empty()),
        &bound,
        &pending.delivered,
        &pending.failed,
        &pending.errors,
    );
    match client.runner_heartbeat(body).await {
        Ok(hb) => {
            *pending = Pending::default();
            Ok(hb)
        }
        // A cursor behind the hub's: the others were applied; carry on.
        Err(nexus_core::Error::Conflict(_)) => {
            *pending = Pending::default();
            client
                .runner_heartbeat(serde_json::json!({ "cli_version": env!("CARGO_PKG_VERSION") }))
                .await
        }
        Err(e) => Err(e),
    }
}

/// Read the feed from the cursor, record the notices, then move the cursor.
async fn poll(
    client: &NexusClient,
    file: &RunnerFile,
    queue: &Queue,
    state: &mut RunnerState,
    active: &BTreeSet<String>,
    bindings: &[RunnerBinding],
) -> Result<usize, String> {
    let mut recorded = 0;
    for _ in 0..MAX_PAGES {
        let since = state.cursor.unwrap_or(0);
        let page = client
            .dispatch_events(None, since, EVENT_PAGE)
            .await
            .map_err(|e| super::dispatch_status::error_text(&e))?;
        let mut dispatches: BTreeMap<String, Option<DispatchGetResponse>> = BTreeMap::new();
        for c in candidates(&page.events, active) {
            if !dispatches.contains_key(&c.event.dispatch_id) {
                let d = match client.dispatch_get(&c.event.dispatch_id).await {
                    Ok(d) => Some(d),
                    Err(e) if e.is_transport() => {
                        return Err(super::dispatch_status::error_text(&e));
                    }
                    // Gone or not readable: a notice without the title.
                    Err(_) => None,
                };
                dispatches.insert(c.event.dispatch_id.clone(), d);
            }
            let mode = bindings
                .iter()
                .find(|b| b.project_id == c.project_id)
                .and_then(|b| b.mode_effective.clone())
                .unwrap_or_else(|| "notify".into());
            let d = dispatches
                .get(&c.event.dispatch_id)
                .and_then(Option::as_ref);
            if let Some(n) = build_notice(&c, d, &mode, &|id| file.name_of(id)) {
                if queue
                    .record(&n)
                    .map_err(|e| format!("notice queue: {}", e.kind()))?
                {
                    recorded += 1;
                    state
                        .projects
                        .entry(n.project_id.clone())
                        .or_default()
                        .queued += 1;
                    log(&format!("{} {}", style("notice").cyan(), n.text));
                }
            }
        }
        // The notices of this page are on disk: now the cursor may move.
        state.cursor = Some(page.next_cursor.max(since));
        if !page.has_more {
            break;
        }
    }
    Ok(recorded)
}

/// Push pending notices into the matching running sessions; without a
/// session (or the inbox) send one desktop notification per notice.
#[allow(clippy::too_many_arguments)]
async fn deliver(
    file: &RunnerFile,
    queue: &Queue,
    adapter: &ClaudeAdapter,
    sessions_dir: &Path,
    home: &Path,
    active: &BTreeSet<String>,
    desktop: bool,
    state: &mut RunnerState,
    pending: &mut Pending,
) {
    let running = sessions(sessions_dir);
    let now = now_ms();
    for project in &file.projects {
        if !active.contains(&project.project_id) {
            continue;
        }
        let pid = &project.project_id;
        let targets: Vec<(&SessionFile, PathBuf)> = running
            .iter()
            .map(|s| (s, session_root(s)))
            .filter(|(_, root)| {
                project
                    .repo_path_hash
                    .as_deref()
                    .is_some_and(|h| h == repo_path_hash(root))
            })
            .collect();
        let done = queue.deliveries(pid);
        let delivered =
            |id: &str, session: &str| done.iter().any(|d| d.id == id && d.session_id == session);
        for n in queue.notices(pid) {
            let fresh = super::observer::parse_rfc3339(&n.created_at)
                .is_some_and(|t| now - t <= DELIVERY_WINDOW_MS);
            if !fresh {
                continue;
            }
            let mut reached = false;
            for (s, root) in &targets {
                if delivered(&n.id, &s.session_id) {
                    reached = true;
                    continue;
                }
                let policy = inbound_policy(&inbound_sources(root, home));
                if matches!(policy, Inbound::Hold | Inbound::Refuse) {
                    // The session holds or refuses: nothing is pushed, the
                    // notice stays queued for its catch-up.
                    continue;
                }
                match adapter.notify_session(s, &n.text).await {
                    Ok(()) => {
                        reached = true;
                        let _ = queue.mark(
                            pid,
                            &Delivery {
                                id: n.id.clone(),
                                session_id: s.session_id.clone(),
                                via: dispatch_runner::CROSS_SESSION_INBOX.into(),
                                at: rfc3339_now(),
                            },
                        );
                        *pending.delivered.entry(pid.clone()).or_default() += 1;
                        state.projects.entry(pid.clone()).or_default().delivered += 1;
                    }
                    Err(reason) => {
                        *pending.failed.entry(pid.clone()).or_default() += 1;
                        pending.errors.insert(pid.clone(), Some(reason.clone()));
                        state.projects.entry(pid.clone()).or_default().failed += 1;
                    }
                }
            }
            if !reached
                && targets.is_empty()
                && desktop
                && !delivered(&n.id, "desktop")
                && desktop_notify(&n.text)
            {
                let _ = queue.mark(
                    pid,
                    &Delivery {
                        id: n.id.clone(),
                        session_id: "desktop".into(),
                        via: "desktop".into(),
                        at: rfc3339_now(),
                    },
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "00000000-0000-4000-8000-0000000000aa";
    const Q: &str = "00000000-0000-4000-8000-0000000000bb";

    fn ev(id: i64, kind: &str, source: &str, target: &str) -> DispatchEvent {
        DispatchEvent {
            id,
            dispatch_id: format!("d{id:07}-0000-4000-8000-000000000000"),
            source_project_id: Some(source.into()),
            target_project_id: Some(target.into()),
            previous_target_project_id: None,
            event_type: kind.into(),
            status_from: None,
            status_to: None,
            message_id: None,
            message_type: None,
            created_at: None,
            bound_project_ids: vec![],
        }
    }

    fn active() -> BTreeSet<String> {
        [P.to_string()].into()
    }

    #[test]
    fn test_notice_triggers() {
        assert_eq!(
            trigger(&ev(1, "created", Q, P), P),
            Some(NoticeKind::Incoming)
        );
        assert_eq!(
            trigger(&ev(1, "created", P, Q), P),
            None,
            "our own new Dispatch"
        );
        let mut s = ev(2, "status_changed", P, Q);
        s.status_to = Some("needs_review".into());
        assert_eq!(trigger(&s, P), Some(NoticeKind::NeedsReply));
        s.status_to = Some("resolved".into());
        assert_eq!(trigger(&s, P), Some(NoticeKind::Resolved));
        s.status_to = Some("in_progress".into());
        assert_eq!(trigger(&s, P), None);
        let mut m = ev(3, "message_added", P, Q);
        m.message_type = Some("reply".into());
        assert_eq!(trigger(&m, P), Some(NoticeKind::Reply));
        m.message_type = Some("progress".into());
        assert_eq!(trigger(&m, P), None);
        // A reply on an incoming Dispatch is our own work, no notice.
        let mut inc = ev(4, "message_added", Q, P);
        inc.message_type = Some("reply".into());
        assert_eq!(trigger(&inc, P), None);
    }

    #[test]
    fn test_one_notice_per_dispatch_in_a_batch() {
        let mut reply = ev(10, "message_added", P, Q);
        reply.message_type = Some("reply".into());
        let mut status = ev(11, "status_changed", P, Q);
        status.dispatch_id = reply.dispatch_id.clone();
        status.status_to = Some("needs_review".into());
        let other = ev(12, "created", Q, P);
        let unbound = ev(13, "created", Q, "00000000-0000-4000-8000-0000000000cc");
        let c = candidates(&[reply, status, other, unbound], &active());
        assert_eq!(c.len(), 2);
        assert_eq!((c[0].kind, c[0].event.id), (NoticeKind::NeedsReply, 11));
        assert_eq!(c[1].kind, NoticeKind::Incoming);
    }

    #[test]
    fn test_notice_text_has_no_body_and_skips_own_replies() {
        let d: DispatchGetResponse = serde_json::from_value(serde_json::json!({
            "dispatch": {"id": "x", "source_project_id": P, "target_project_id": Q,
                "title": "Fix \"the\"\nthing", "type": "bug_report", "priority": "high",
                "status": "open", "created_at": "2026-11-07T00:00:00Z"},
            "messages": [
                {"id": "m1", "author_project_id": P, "message_type": "reply", "body_markdown": "SECRET BODY"},
                {"id": "m2", "author_project_id": Q, "message_type": "reply", "body_markdown": "SECRET BODY"}
            ]
        }))
        .unwrap();
        let names = |id: &str| {
            if id == Q {
                "NEXUS-APP".to_string()
            } else {
                "NEXUS-CLI".to_string()
            }
        };
        let mut m = ev(20, "message_added", P, Q);
        m.message_type = Some("reply".into());
        m.message_id = Some("m1".into());
        let own = Candidate {
            project_id: P.into(),
            kind: NoticeKind::Reply,
            event: m.clone(),
        };
        assert!(build_notice(&own, Some(&d), "notify", &names).is_none());
        m.message_id = Some("m2".into());
        let theirs = Candidate {
            project_id: P.into(),
            kind: NoticeKind::Reply,
            event: m,
        };
        let n = build_notice(&theirs, Some(&d), "notify", &names).unwrap();
        assert!(n.text.starts_with(
            "Nexus: NEXUS-APP added a reply on Dispatch d0000020 \"Fix  the  thing\""
                .replace("  ", " ")
                .as_str()
        ));
        assert!(!n.text.contains("SECRET"));
        assert_eq!(n.id, notice_id(P, 20));

        let mut s = ev(21, "status_changed", P, Q);
        s.status_to = Some("waiting_for_requester".into());
        let c = Candidate {
            project_id: P.into(),
            kind: NoticeKind::NeedsReply,
            event: s,
        };
        let n = build_notice(&c, Some(&d), "notify", &names).unwrap();
        assert!(n.text.ends_with(
            "from NEXUS-APP needs your reply (waiting_for_requester). Run /nexus-dispatch-sweep."
        ));
    }

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nexus-listen-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_queue_is_idempotent_per_event() {
        let dir = temp("queue");
        let q = Queue::new(dir.clone());
        let n = Notice {
            id: notice_id(P, 7),
            project_id: P.into(),
            dispatch_id: "d".into(),
            event_id: 7,
            kind: NoticeKind::Incoming,
            mode: "notify".into(),
            text: "Nexus: x".into(),
            created_at: rfc3339_now(),
        };
        assert!(q.record(&n).unwrap());
        assert!(!q.record(&n).unwrap(), "a replayed event queues nothing");
        assert_eq!(q.notices(P).len(), 1);
        q.mark(
            P,
            &Delivery {
                id: n.id.clone(),
                session_id: "s1".into(),
                via: "x".into(),
                at: "t".into(),
            },
        )
        .unwrap();
        assert_eq!(q.deliveries(P).len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_inbound_policy_takes_the_strictest_value() {
        let dir = temp("inbound");
        let user = dir.join("user.json");
        let repo = dir.join("repo.json");
        fs::write(&user, r#"{"crossSessionInbound":"accept"}"#).unwrap();
        assert_eq!(
            inbound_policy(&[user.clone(), dir.join("missing.json")]),
            Inbound::Accept
        );
        fs::write(&repo, r#"{"crossSessionInbound":"hold"}"#).unwrap();
        assert_eq!(inbound_policy(&[user.clone(), repo.clone()]), Inbound::Hold);
        fs::write(&repo, r#"{"crossSessionInbound":"refuse"}"#).unwrap();
        assert_eq!(
            inbound_policy(&[user.clone(), repo.clone()]),
            Inbound::Refuse
        );
        fs::write(&repo, r#"{"crossSessionInbound":"sometimes"}"#).unwrap();
        assert_eq!(inbound_policy(std::slice::from_ref(&repo)), Inbound::Hold);
        fs::write(&repo, r#"{"other":1}"#).unwrap();
        assert_eq!(inbound_policy(&[repo]), Inbound::Unset);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_inbox_frames_are_two_json_lines() {
        let frames = inbox_frames("tok", "Nexus: \"hi\"");
        let lines: Vec<serde_json::Value> = frames
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(
            lines[0],
            serde_json::json!({"type": "auth", "token": "tok"})
        );
        assert_eq!(lines[1]["type"], "user");
        assert_eq!(lines[1]["message"]["content"], "Nexus: \"hi\"");
    }

    #[test]
    fn test_heartbeat_report_shape() {
        let bound = vec![P.to_string()];
        let mut delivered = BTreeMap::new();
        delivered.insert(P.to_string(), 3);
        delivered.insert(Q.to_string(), 1); // not bound: left out
        let mut errors = BTreeMap::new();
        errors.insert(P.to_string(), None);
        let body = heartbeat_report(Some(42), &bound, &delivered, &BTreeMap::new(), &errors);
        assert_eq!(body["cursors"][P], 42);
        assert_eq!(body["delivered"][P], 3);
        assert!(body["delivered"].get(Q).is_none());
        assert!(body.get("failed").is_none());
        assert!(body["errors"][P].is_null());
        let none = heartbeat_report(
            None,
            &bound,
            &BTreeMap::new(),
            &BTreeMap::new(),
            &BTreeMap::new(),
        );
        assert!(none.get("cursors").is_none());
    }

    #[test]
    fn test_start_cursor_prefers_the_hub_cursor() {
        let b = |cursor: Option<i64>, latest: Option<i64>| RunnerBinding {
            project_id: P.into(),
            cursor,
            latest_event_id: latest,
            ..Default::default()
        };
        let hb = |p: Vec<RunnerBinding>| RunnerHeartbeatResponse {
            projects: p,
            ..Default::default()
        };
        assert_eq!(start_cursor(&hb(vec![b(None, Some(90))])), 90);
        assert_eq!(
            start_cursor(&hb(vec![b(Some(40), Some(90)), b(Some(50), None)])),
            40
        );
        assert_eq!(start_cursor(&hb(vec![])), 0);
    }

    #[test]
    fn test_second_lock_fails() {
        let dir = temp("lock");
        let path = dir.join("runner.lock");
        let first = lock(&path).unwrap();
        assert!(lock(&path).is_err());
        drop(first);
        assert!(lock(&path).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_notify_session_writes_the_frames() {
        let dir = temp("sock");
        let path = dir.join("s.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let got = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = String::new();
            s.read_to_string(&mut buf).await.unwrap();
            buf
        });
        let session = SessionFile {
            session_id: "s1".into(),
            cwd: dir.display().to_string(),
            socket: path.display().to_string(),
            token: "tok".into(),
        };
        ClaudeAdapter { inbox: true }
            .notify_session(&session, "Nexus: hi")
            .await
            .unwrap();
        assert_eq!(got.await.unwrap(), inbox_frames("tok", "Nexus: hi"));
        assert!(ClaudeAdapter { inbox: false }
            .notify_session(&session, "x")
            .await
            .is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    // -- A local hub stub for the end-to-end cases ---------------------------

    #[derive(Default)]
    struct Hub {
        events: Vec<serde_json::Value>,
        /// Cursor the hub holds for the binding (last one reported).
        cursor: Option<i64>,
        heartbeats: Vec<serde_json::Value>,
        /// Automation switched off on the platform.
        off: bool,
    }

    type SharedHub = std::sync::Arc<std::sync::Mutex<Hub>>;

    fn respond(hub: &SharedHub, body: &serde_json::Value) -> serde_json::Value {
        let mut h = hub.lock().unwrap();
        match body["action"].as_str().unwrap_or("") {
            "runner_heartbeat" => {
                if let Some(c) = body["cursors"][P].as_i64() {
                    h.cursor = Some(c);
                }
                h.heartbeats.push(body.clone());
                let latest = h.events.iter().filter_map(|e| e["id"].as_i64()).max();
                let (mode, by) = if h.off {
                    ("off", Some("platform_disabled"))
                } else {
                    ("notify", None)
                };
                serde_json::json!({"runner_id": "r1", "paused": false, "platform_enabled": !h.off,
                    "projects": [{"project_id": P, "mode_requested": "notify", "mode_effective": mode,
                        "blocked_by": by, "writes_allowed": false, "cursor": h.cursor, "latest_event_id": latest}]})
            }
            "dispatch_events" => {
                let since = body["since"].as_i64().unwrap_or(0);
                let events: Vec<_> = h
                    .events
                    .iter()
                    .filter(|e| e["id"].as_i64().unwrap() > since)
                    .cloned()
                    .collect();
                let next = events
                    .last()
                    .and_then(|e| e["id"].as_i64())
                    .unwrap_or(since);
                serde_json::json!({"projects": [P], "since": since, "next_cursor": next, "has_more": false, "events": events})
            }
            "dispatch_get" => serde_json::json!({
                "dispatch": {"id": body["dispatch_id"], "source_project_id": Q, "target_project_id": P,
                    "title": "Stub title", "type": "question", "priority": "normal", "status": "open",
                    "created_at": "2026-11-07T00:00:00Z"},
                "messages": []}),
            _ => serde_json::json!({"error": "unknown"}),
        }
    }

    async fn serve(hub: SharedHub) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let hub = hub.clone();
                tokio::spawn(async move {
                    let mut req = Vec::new();
                    let mut buf = [0u8; 8192];
                    loop {
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        req.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&req).to_string();
                        let Some(end) = text.find("\r\n\r\n") else {
                            continue;
                        };
                        let len = text[..end]
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if req.len() < end + 4 + len {
                            continue;
                        }
                        let body: serde_json::Value =
                            serde_json::from_slice(&req[end + 4..end + 4 + len])
                                .unwrap_or_default();
                        let out = respond(&hub, &body).to_string();
                        let resp = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}", out.len());
                        let _ = sock.write_all(resp.as_bytes()).await;
                        return;
                    }
                });
            }
        });
        format!("http://127.0.0.1:{}", addr.port())
    }

    fn created(id: i64) -> serde_json::Value {
        serde_json::json!({"id": id, "dispatch_id": format!("{id:08}-0000-4000-8000-000000000000"),
            "source_project_id": Q, "target_project_id": P, "event_type": "created", "bound_project_ids": [P]})
    }

    /// An event created while the runner
    /// is down arrives once after the restart; a replay or a lost state
    /// queues nothing twice; the cursor reaches the hub.
    #[tokio::test]
    async fn test_restart_and_replay_queue_each_notice_once() {
        let dir = temp("accept");
        let repo = dir.join("repo");
        fs::create_dir_all(&repo).unwrap();
        let hub: SharedHub = Default::default();
        hub.lock().unwrap().events.push(created(1)); // history before the runner
        let url = serve(hub.clone()).await;
        let runner_file = dir.join("dispatch-runner.json");
        let file = RunnerFile {
            runner_id: "r1".into(),
            token: "nxs_run_test".into(),
            token_prefix: None,
            token_expires_at: None,
            api_url: url,
            name: "test".into(),
            registered_at: rfc3339_now(),
            projects: vec![dispatch_runner::BoundProject {
                project_id: P.into(),
                mode: "notify".into(),
                runtime: "claude_code".into(),
                repo_path_hash: Some(repo_path_hash(&repo)),
            }],
            project_names: [(Q.to_string(), "NEXUS-APP".to_string())].into(),
            runtimes: vec![],
        };
        write_private(&runner_file, &serde_json::to_string(&file).unwrap()).unwrap();
        let state_dir = dir.join("state");
        let env = || ListenEnv {
            runner_file: runner_file.clone(),
            state_dir: state_dir.clone(),
            home: dir.clone(),
            claude_inbox: true,
            desktop: false,
            embedded: false,
        };
        let once = || ListenOptions {
            interval_secs: 10,
            heartbeat_secs: 60,
            once: true,
        };
        let queue = Queue::new(state_dir.join("notices"));

        // First start: no backlog flood, history is skipped.
        run_loop(once(), env()).await.unwrap();
        assert!(queue.notices(P).is_empty());

        // Runner down; a Dispatch arrives; a session of the repo is running.
        hub.lock().unwrap().events.push(created(2));
        let sock_path = dir.join("s.sock");
        let listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
        let got = tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = String::new();
            s.read_to_string(&mut buf).await.unwrap();
            buf
        });
        fs::create_dir_all(state_dir.join("sessions")).unwrap();
        fs::write(
            state_dir.join("sessions").join("s1.json"),
            serde_json::json!({"session_id": "s1", "cwd": repo.display().to_string(),
                "socket": sock_path.display().to_string(), "token": "msgtok", "started_at": "x"})
            .to_string(),
        )
        .unwrap();
        run_loop(once(), env()).await.unwrap();
        let notices = queue.notices(P);
        assert_eq!(notices.len(), 1);
        assert!(
            notices[0].text.contains("from NEXUS-APP"),
            "{}",
            notices[0].text
        );
        let frames = got.await.unwrap();
        assert!(frames.contains("\"token\":\"msgtok\"") && frames.contains("Stub title"));
        assert_eq!(queue.deliveries(P).len(), 1);
        let state: RunnerState =
            serde_json::from_str(&fs::read_to_string(state_dir.join("runner.json")).unwrap())
                .unwrap();
        assert_eq!(state.cursor, Some(2));
        assert!(!fs::read_to_string(state_dir.join("runner.json"))
            .unwrap()
            .contains("msgtok"));
        assert_eq!(
            hub.lock().unwrap().cursor,
            Some(2),
            "the cursor reached the hub"
        );

        // Restart: nothing new, nothing twice (the session socket is gone).
        run_loop(once(), env()).await.unwrap();
        assert_eq!(queue.notices(P).len(), 1);

        // Lost local state: the hub cursor is the start, no duplicate.
        fs::remove_file(state_dir.join("runner.json")).unwrap();
        hub.lock().unwrap().cursor = Some(1); // even a cursor behind replays safely
        run_loop(once(), env()).await.unwrap();
        assert_eq!(queue.notices(P).len(), 1);
        let _ = fs::remove_dir_all(&dir);
    }

    /// Started while the platform switch is off: no cursor is fixed, so
    /// switching it on later does not replay the history.
    #[tokio::test]
    async fn test_blocked_start_does_not_replay_history() {
        let dir = temp("blocked");
        let hub: SharedHub = Default::default();
        {
            let mut h = hub.lock().unwrap();
            h.off = true;
            h.events.extend([created(1), created(2)]);
        }
        let url = serve(hub.clone()).await;
        let runner_file = dir.join("dispatch-runner.json");
        let file = RunnerFile {
            runner_id: "r1".into(),
            token: "nxs_run_test".into(),
            token_prefix: None,
            token_expires_at: None,
            api_url: url,
            name: "test".into(),
            registered_at: rfc3339_now(),
            projects: vec![dispatch_runner::BoundProject {
                project_id: P.into(),
                mode: "notify".into(),
                runtime: "claude_code".into(),
                repo_path_hash: None,
            }],
            project_names: Default::default(),
            runtimes: vec![],
        };
        write_private(&runner_file, &serde_json::to_string(&file).unwrap()).unwrap();
        let state_dir = dir.join("state");
        let env = || ListenEnv {
            runner_file: runner_file.clone(),
            state_dir: state_dir.clone(),
            home: dir.clone(),
            claude_inbox: true,
            desktop: false,
            embedded: false,
        };
        let once = || ListenOptions {
            interval_secs: 10,
            heartbeat_secs: 60,
            once: true,
        };
        run_loop(once(), env()).await.unwrap();
        let state: RunnerState =
            serde_json::from_str(&fs::read_to_string(state_dir.join("runner.json")).unwrap())
                .unwrap();
        assert_eq!(state.state, "blocked");
        assert_eq!(state.cursor, None);
        // Switched on, plus a new Dispatch: only that one is a notice.
        {
            let mut h = hub.lock().unwrap();
            h.off = false;
        }
        run_loop(once(), env()).await.unwrap();
        hub.lock().unwrap().events.push(created(3));
        run_loop(once(), env()).await.unwrap();
        let queue = Queue::new(state_dir.join("notices"));
        let ids: Vec<i64> = queue.notices(P).iter().map(|n| n.event_id).collect();
        assert_eq!(ids, [3]);
        let _ = fs::remove_dir_all(&dir);
    }

    /// A session that holds inbound messages gets nothing
    /// pushed; the notice stays queued.
    #[tokio::test]
    async fn test_holding_session_gets_nothing_pushed() {
        let dir = temp("hold");
        let repo = dir.join("repo");
        fs::create_dir_all(repo.join(".claude")).unwrap();
        fs::write(
            repo.join(".claude").join("settings.json"),
            r#"{"crossSessionInbound":"hold"}"#,
        )
        .unwrap();
        let hub: SharedHub = Default::default();
        let url = serve(hub.clone()).await;
        let runner_file = dir.join("dispatch-runner.json");
        let file = RunnerFile {
            runner_id: "r1".into(),
            token: "nxs_run_test".into(),
            token_prefix: None,
            token_expires_at: None,
            api_url: url,
            name: "test".into(),
            registered_at: rfc3339_now(),
            projects: vec![dispatch_runner::BoundProject {
                project_id: P.into(),
                mode: "notify".into(),
                runtime: "claude_code".into(),
                repo_path_hash: Some(repo_path_hash(&repo)),
            }],
            project_names: Default::default(),
            runtimes: vec![],
        };
        write_private(&runner_file, &serde_json::to_string(&file).unwrap()).unwrap();
        let state_dir = dir.join("state");
        let env = || ListenEnv {
            runner_file: runner_file.clone(),
            state_dir: state_dir.clone(),
            home: dir.clone(),
            claude_inbox: true,
            desktop: false,
            embedded: false,
        };
        run_loop(
            ListenOptions {
                interval_secs: 10,
                heartbeat_secs: 60,
                once: true,
            },
            env(),
        )
        .await
        .unwrap();
        hub.lock().unwrap().events.push(created(5));
        let sock_path = dir.join("h.sock");
        let _listener = tokio::net::UnixListener::bind(&sock_path).unwrap();
        fs::create_dir_all(state_dir.join("sessions")).unwrap();
        fs::write(
            state_dir.join("sessions").join("s2.json"),
            serde_json::json!({"session_id": "s2", "cwd": repo.display().to_string(),
                "socket": sock_path.display().to_string(), "token": "t"})
            .to_string(),
        )
        .unwrap();
        run_loop(
            ListenOptions {
                interval_secs: 10,
                heartbeat_secs: 60,
                once: true,
            },
            env(),
        )
        .await
        .unwrap();
        let queue = Queue::new(state_dir.join("notices"));
        assert_eq!(queue.notices(P).len(), 1, "queued");
        assert!(queue.deliveries(P).is_empty(), "nothing pushed");
        let _ = fs::remove_dir_all(&dir);
    }
}
