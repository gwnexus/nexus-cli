//! Install the Claude Code plugins Nexus manages for a project (NEXUS-APP
//! dispatch ff608ed4, ADR-0117 Tier A).
//!
//! `af_export.claude_settings.values.enabledPlugins` (and
//! `extraKnownMarketplaces`) are merged into `.claude/settings.json` by
//! every pull. That only *enables* a plugin; Claude Code still has to have
//! it installed. `nexus pull --force` and `nexus run` call [`sync`], which:
//!
//! 1. reads `claude plugin list --json` and treats a plugin as present when
//!    it is installed at user scope, or at project/local scope for *this*
//!    workspace;
//! 2. for marketplaces a missing plugin needs, adds the ones declared in
//!    `extraKnownMarketplaces` (`claude plugin marketplace add --scope
//!    project`) or refreshes an already registered one;
//! 3. runs `claude plugin install <id> --scope project --json` per missing
//!    plugin -- never with `--yes` / `--accept-command`: a plugin that
//!    declares an install command needs a person to confirm it;
//! 4. records what it installed in the CCX lock (provenance), and forgets
//!    lock entries for plugins Nexus no longer manages. Disabling those is
//!    the settings merge's job: it removes only `enabledPlugins` entries
//!    Nexus wrote (see [`super::ccx::reconcile_settings_removed_keys`]).
//!
//! User- and local-scope plugins are never touched. Every failure is
//! reported per plugin and never blocks a pull or a launch.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use console::style;
use nexus_core::api::ClaudeSettingsSpec;

use super::ccx::CcxLockPluginEntry;

/// Opt-out for CI and air-gapped machines: skip the whole plugin sync.
pub const SKIP_ENV: &str = "NEXUS_SKIP_PLUGIN_SYNC";

/// Upper bound for a single `claude plugin ...` call (a marketplace clone
/// can take a while; Claude Code's own clone timeout is 120 s).
const COMMAND_TIMEOUT: Duration = Duration::from_secs(180);

/// Result of one `claude` invocation.
#[derive(Debug, Clone, Default)]
pub struct CliOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs `claude` with the given arguments in the workspace. A trait so the
/// sync logic can be tested without a real Claude Code.
pub trait ClaudeCli {
    /// `Err` when `claude` could not be started or timed out.
    fn run(&self, args: &[&str]) -> Result<CliOutput, String>;
}

/// The real `claude` binary, run in `workspace` with stdin closed (so a
/// confirmation prompt fails instead of hanging) and a timeout.
pub struct SystemClaude {
    pub workspace: PathBuf,
}

impl ClaudeCli for SystemClaude {
    fn run(&self, args: &[&str]) -> Result<CliOutput, String> {
        let mut child = Command::new("claude")
            .args(args)
            .current_dir(&self.workspace)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not start claude: {e}"))?;
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();
        let out_reader = std::thread::spawn(move || {
            let mut buf = String::new();
            if let Some(p) = stdout_pipe.as_mut() {
                let _ = p.read_to_string(&mut buf);
            }
            buf
        });
        let err_reader = std::thread::spawn(move || {
            let mut buf = String::new();
            if let Some(p) = stderr_pipe.as_mut() {
                let _ = p.read_to_string(&mut buf);
            }
            buf
        });
        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if start.elapsed() > COMMAND_TIMEOUT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "claude {} timed out after {}s",
                        args.first().copied().unwrap_or_default(),
                        COMMAND_TIMEOUT.as_secs()
                    ));
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(e) => return Err(format!("waiting for claude failed: {e}")),
            }
        };
        Ok(CliOutput {
            success: status.success(),
            stdout: out_reader.join().unwrap_or_default(),
            stderr: err_reader.join().unwrap_or_default(),
        })
    }
}

/// An entry of `claude plugin list --json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledPlugin {
    pub id: String,
    pub scope: Option<String>,
    pub project_path: Option<String>,
}

/// What happened to one managed plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PluginState {
    /// Installed by this run (project scope).
    Installed,
    /// Already installed (user scope, or project/local scope here).
    Present,
    /// Install failed; the reason is shown, the launch goes on.
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginOutcome {
    pub id: String,
    pub state: PluginState,
}

/// What happened to one marketplace a missing plugin needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarketplaceState {
    /// Registered from `extraKnownMarketplaces` (project scope).
    Added,
    /// Already registered; catalog refreshed before installing.
    Updated,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarketplaceOutcome {
    pub name: String,
    pub state: MarketplaceState,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub marketplaces: Vec<MarketplaceOutcome>,
    pub plugins: Vec<PluginOutcome>,
    /// Plugins Nexus installed earlier and no longer manages; their
    /// project-scope `enabledPlugins` entry is removed by the settings
    /// merge, the lock entry is dropped here.
    pub released: Vec<String>,
}

/// Plugin ids the spec enables (`enabledPlugins: { id: true }`), sorted.
pub fn managed_plugins(spec: Option<&ClaudeSettingsSpec>) -> Vec<String> {
    spec.filter(|s| s.managed_keys.iter().any(|k| k == "enabledPlugins"))
        .and_then(|s| s.value("enabledPlugins"))
        .and_then(|v| v.as_object())
        .map(|m| {
            m.iter()
                .filter(|(_, on)| on.as_bool() == Some(true))
                .map(|(id, _)| id.clone())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect()
        })
        .unwrap_or_default()
}

/// `extraKnownMarketplaces` of the spec, by marketplace name.
fn declared_marketplaces(
    spec: Option<&ClaudeSettingsSpec>,
) -> serde_json::Map<String, serde_json::Value> {
    spec.filter(|s| s.managed_keys.iter().any(|k| k == "extraKnownMarketplaces"))
        .and_then(|s| s.value("extraKnownMarketplaces"))
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default()
}

/// The marketplace part of `name@marketplace`.
fn marketplace_of(id: &str) -> Option<&str> {
    id.split_once('@').map(|(_, m)| m).filter(|m| !m.is_empty())
}

/// The `claude plugin marketplace add` source argument for an
/// `extraKnownMarketplaces` entry (`{ source: { source, repo|url|path, ref } }`).
pub fn marketplace_source_arg(entry: &serde_json::Value) -> Option<String> {
    let source = entry.get("source")?;
    let kind = source.get("source")?.as_str()?;
    let with_ref = |base: &str| match source.get("ref").and_then(|r| r.as_str()) {
        Some(r) if !r.is_empty() => format!("{base}#{r}"),
        _ => base.to_string(),
    };
    match kind {
        "github" => Some(with_ref(source.get("repo")?.as_str()?)),
        "git" => Some(with_ref(source.get("url")?.as_str()?)),
        "url" => Some(source.get("url")?.as_str()?.to_string()),
        "directory" | "file" => Some(source.get("path")?.as_str()?.to_string()),
        _ => None,
    }
}

/// Parse `claude plugin list --json` (an array of `{ id, scope,
/// projectPath }`; older shapes from [`super::claude_cmd`] are accepted
/// with unknown scope).
pub fn parse_installed(value: &serde_json::Value) -> Option<Vec<InstalledPlugin>> {
    let items = match value {
        serde_json::Value::Array(items) => items.clone(),
        serde_json::Value::Object(map) => match map.get("plugins") {
            Some(serde_json::Value::Array(items)) => items.clone(),
            _ => map.keys().map(|k| serde_json::json!({ "id": k })).collect(),
        },
        _ => return None,
    };
    Some(
        items
            .iter()
            .filter_map(|item| {
                let id = match item.get("id").and_then(|v| v.as_str()) {
                    Some(id) => id.to_string(),
                    None => {
                        let name = item.get("name")?.as_str()?;
                        match item.get("marketplace").and_then(|v| v.as_str()) {
                            Some(m) => format!("{name}@{m}"),
                            None => name.to_string(),
                        }
                    }
                };
                Some(InstalledPlugin {
                    id,
                    scope: item
                        .get("scope")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                    project_path: item
                        .get("projectPath")
                        .and_then(|v| v.as_str())
                        .map(str::to_string),
                })
            })
            .collect(),
    )
}

fn same_dir(a: &Path, b: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    canon(a) == canon(b)
}

/// Whether `id` is usable in `workspace`: installed at user scope, or at
/// project/local scope for this workspace. An entry without scope (older
/// Claude Code) counts as present.
pub fn is_present(installed: &[InstalledPlugin], id: &str, workspace: &Path) -> bool {
    installed.iter().any(|p| {
        let id_matches =
            p.id == id || (!p.id.contains('@') && Some(p.id.as_str()) == id.split('@').next());
        id_matches
            && match p.scope.as_deref() {
                None | Some("user") => true,
                Some(_) => p
                    .project_path
                    .as_deref()
                    .is_some_and(|pp| same_dir(Path::new(pp), workspace)),
            }
    })
}

/// The scope `id` is installed at for `workspace`, for `nexus status`.
pub fn present_scope(installed: &[InstalledPlugin], id: &str, workspace: &Path) -> Option<String> {
    installed
        .iter()
        .filter(|p| p.id == id)
        .find(|p| is_present(std::slice::from_ref(*p), id, workspace))
        .map(|p| p.scope.clone().unwrap_or_else(|| "unknown".to_string()))
}

/// `claude plugin list --json`, `None` when unavailable.
pub fn list_installed(cli: &dyn ClaudeCli) -> Option<Vec<InstalledPlugin>> {
    let out = cli.run(&["plugin", "list", "--json"]).ok()?;
    if !out.success {
        return None;
    }
    parse_installed(&serde_json::from_str(out.stdout.trim()).ok()?)
}

/// Registered marketplace names (`claude plugin marketplace list --json`).
fn list_marketplaces(cli: &dyn ClaudeCli) -> Option<BTreeSet<String>> {
    let out = cli.run(&["plugin", "marketplace", "list", "--json"]).ok()?;
    if !out.success {
        return None;
    }
    let value: serde_json::Value = serde_json::from_str(out.stdout.trim()).ok()?;
    Some(
        value
            .as_array()?
            .iter()
            .filter_map(|m| m.get("name").and_then(|n| n.as_str()).map(str::to_string))
            .collect(),
    )
}

/// The last JSON object line of a `--json` run (Claude Code prints one
/// result line, possibly after progress output).
fn json_result(stdout: &str) -> Option<serde_json::Value> {
    stdout
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str::<serde_json::Value>(l.trim()).ok())
        .filter(|v| v.is_object())
}

/// Short failure reason from a `--json` result or stderr.
fn failure_reason(out: &CliOutput) -> String {
    let from_json = json_result(&out.stdout).and_then(|v| {
        let message = v
            .get("message")
            .and_then(|m| m.as_str())
            .map(str::to_string);
        if v.get("shownCommand").is_some() {
            return Some(
                "declares an install command that needs confirmation; \
                 run claude plugin install interactively"
                    .to_string(),
            );
        }
        message
    });
    let text = from_json.unwrap_or_else(|| {
        let err = out.stderr.trim();
        if err.is_empty() {
            out.stdout.trim().to_string()
        } else {
            err.to_string()
        }
    });
    let line = text
        .lines()
        .map(|l| l.trim().trim_start_matches('✘').trim())
        .find(|l| !l.is_empty())
        .unwrap_or("unknown error")
        .to_string();
    if line.chars().count() > 160 {
        format!("{}…", line.chars().take(160).collect::<String>())
    } else {
        line
    }
}

/// Install the managed plugins that are missing; see the module docs.
/// `previously_installed` is the CCX lock's plugin provenance; the
/// returned map is the new provenance to record.
pub fn sync(
    cli: &dyn ClaudeCli,
    workspace: &Path,
    spec: Option<&ClaudeSettingsSpec>,
    previously_installed: &BTreeMap<String, CcxLockPluginEntry>,
) -> (SyncReport, BTreeMap<String, CcxLockPluginEntry>) {
    let managed = managed_plugins(spec);
    let mut report = SyncReport::default();
    let mut provenance: BTreeMap<String, CcxLockPluginEntry> = previously_installed
        .iter()
        .filter(|(id, _)| managed.contains(id))
        .map(|(id, e)| (id.clone(), e.clone()))
        .collect();
    report.released = previously_installed
        .keys()
        .filter(|id| !managed.contains(id))
        .cloned()
        .collect();
    if managed.is_empty() {
        return (report, provenance);
    }

    let Some(installed) = list_installed(cli) else {
        let reason = "claude plugin list unavailable (is Claude Code installed?)".to_string();
        report.plugins = managed
            .iter()
            .map(|id| PluginOutcome {
                id: id.clone(),
                state: PluginState::Failed(reason.clone()),
            })
            .collect();
        return (report, provenance);
    };

    let missing: Vec<&String> = managed
        .iter()
        .filter(|id| !is_present(&installed, id, workspace))
        .collect();

    // Marketplaces first: register declared ones, refresh known ones so a
    // newly added plugin is found in the catalog.
    let declared = declared_marketplaces(spec);
    let needed: BTreeSet<&str> = missing.iter().filter_map(|id| marketplace_of(id)).collect();
    let mut failed_marketplaces: BTreeMap<String, String> = BTreeMap::new();
    if !needed.is_empty() {
        let registered = list_marketplaces(cli).unwrap_or_default();
        for name in needed {
            let state = if registered.contains(name) {
                match cli.run(&["plugin", "marketplace", "update", name]) {
                    Ok(out) if out.success => MarketplaceState::Updated,
                    // A failed refresh is not fatal: the cached catalog
                    // may still have the plugin.
                    Ok(out) => MarketplaceState::Failed(failure_reason(&out)),
                    Err(e) => MarketplaceState::Failed(e),
                }
            } else if let Some(entry) = declared.get(name) {
                match marketplace_source_arg(entry) {
                    Some(source) => match cli.run(&[
                        "plugin",
                        "marketplace",
                        "add",
                        &source,
                        "--scope",
                        "project",
                        "--json",
                    ]) {
                        Ok(out) if out.success => MarketplaceState::Added,
                        Ok(out) => {
                            let reason = failure_reason(&out);
                            failed_marketplaces.insert(name.to_string(), reason.clone());
                            MarketplaceState::Failed(reason)
                        }
                        Err(e) => {
                            failed_marketplaces.insert(name.to_string(), e.clone());
                            MarketplaceState::Failed(e)
                        }
                    },
                    None => {
                        let reason = "unsupported marketplace source".to_string();
                        failed_marketplaces.insert(name.to_string(), reason.clone());
                        MarketplaceState::Failed(reason)
                    }
                }
            } else {
                // Neither registered nor declared: let the install report it.
                continue;
            };
            report.marketplaces.push(MarketplaceOutcome {
                name: name.to_string(),
                state,
            });
        }
    }

    for id in &managed {
        if !missing.contains(&id) {
            report.plugins.push(PluginOutcome {
                id: id.clone(),
                state: PluginState::Present,
            });
            continue;
        }
        if let Some(reason) = marketplace_of(id).and_then(|m| failed_marketplaces.get(m)) {
            report.plugins.push(PluginOutcome {
                id: id.clone(),
                state: PluginState::Failed(format!("marketplace unavailable: {reason}")),
            });
            continue;
        }
        let state = match cli.run(&["plugin", "install", id, "--scope", "project", "--json"]) {
            Ok(out) => {
                let result = json_result(&out.stdout);
                let ok = out.success
                    && result
                        .as_ref()
                        .and_then(|v| v.get("outcome"))
                        .and_then(|o| o.as_str())
                        .is_none_or(|o| o == "ok");
                let already = result
                    .as_ref()
                    .and_then(|v| v.get("message"))
                    .and_then(|m| m.as_str())
                    .is_some_and(|m| m.contains("already installed"));
                match (ok, already) {
                    (true, true) => PluginState::Present,
                    (true, false) => PluginState::Installed,
                    _ => PluginState::Failed(failure_reason(&out)),
                }
            }
            Err(e) => PluginState::Failed(e),
        };
        if state == PluginState::Installed {
            provenance.insert(
                id.clone(),
                CcxLockPluginEntry {
                    scope: "project".to_string(),
                    installed_at: super::ccx::chrono_like_now(),
                },
            );
        }
        report.plugins.push(PluginOutcome {
            id: id.clone(),
            state,
        });
    }

    (report, provenance)
}

/// [`sync`] against the real `claude`, recording provenance in the CCX
/// lock and printing the report. Never fails: errors are printed.
pub fn sync_workspace(
    workspace: &Path,
    agentic_root: &str,
    spec: Option<&ClaudeSettingsSpec>,
) -> SyncReport {
    if std::env::var(SKIP_ENV).is_ok_and(|v| !v.is_empty() && v != "0") {
        return SyncReport::default();
    }
    let lock = super::ccx::load_lock(workspace, agentic_root);
    let previous = lock.map(|l| l.plugins).unwrap_or_default();
    if managed_plugins(spec).is_empty() && previous.is_empty() {
        return SyncReport::default();
    }
    let cli = SystemClaude {
        workspace: workspace.to_path_buf(),
    };
    let (report, provenance) = sync(&cli, workspace, spec, &previous);
    if let Err(e) = super::ccx::record_plugins_in_lock(workspace, agentic_root, provenance) {
        println!(
            "   {} could not record plugin provenance in the CCX lock: {}",
            style("!").bold().yellow(),
            e
        );
    }
    print_report(&report);
    report
}

/// Managed plugin spec as recorded by the last pull (CCX lock), for
/// `nexus run`, which does not pull.
pub fn locked_spec(workspace: &Path, agentic_root: &str) -> Option<ClaudeSettingsSpec> {
    super::ccx::load_lock(workspace, agentic_root).and_then(|l| l.settings)
}

/// Print the plugin section (pull and run output); nothing when every
/// managed plugin is already present (`nexus status` shows them).
pub fn print_report(report: &SyncReport) {
    // Quiet when there is nothing to do: every launch runs this.
    if report.marketplaces.is_empty()
        && report.released.is_empty()
        && report
            .plugins
            .iter()
            .all(|p| p.state == PluginState::Present)
    {
        return;
    }
    println!();
    println!("{}", style("Claude Code plugins:").bold());
    let label = |s: &str| format!("{s:<11}");
    for m in &report.marketplaces {
        match &m.state {
            MarketplaceState::Added => println!(
                "  {} {}   {}",
                style(label("MARKETPLACE")).green(),
                m.name,
                style("added (project scope)").dim()
            ),
            MarketplaceState::Updated => println!(
                "  {} {}   {}",
                style(label("MARKETPLACE")).dim(),
                m.name,
                style("refreshed").dim()
            ),
            MarketplaceState::Failed(reason) => println!(
                "  {} {}   {}",
                style(label("MARKETPLACE")).yellow(),
                m.name,
                style(reason).yellow()
            ),
        }
    }
    for p in &report.plugins {
        match &p.state {
            PluginState::Installed => println!(
                "  {} {}   {}",
                style(label("INSTALLED")).green(),
                p.id,
                style("project scope").dim()
            ),
            PluginState::Present => println!("  {} {}", style(label("PRESENT")).dim(), p.id),
            PluginState::Failed(reason) => println!(
                "  {} {}   {}",
                style(label("FAILED")).yellow(),
                p.id,
                style(reason).yellow()
            ),
        }
    }
    for id in &report.released {
        println!(
            "  {} {}   {}",
            style(label("RELEASED")).cyan(),
            id,
            style("no longer managed by Nexus (pull removes its project-scope enablement)").dim()
        );
    }
    if report
        .plugins
        .iter()
        .any(|p| matches!(p.state, PluginState::Failed(_)))
    {
        println!(
            "  {}",
            style("Failed plugins do not block the session; install them with /plugin in Claude Code.")
                .dim()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// Scripted `claude`: answers by argument prefix, records every call.
    struct FakeClaude {
        calls: RefCell<Vec<String>>,
        answers: Vec<(&'static str, Result<CliOutput, String>)>,
    }

    impl FakeClaude {
        fn new(answers: Vec<(&'static str, Result<CliOutput, String>)>) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                answers,
            }
        }
        fn calls(&self) -> Vec<String> {
            self.calls.borrow().clone()
        }
    }

    impl ClaudeCli for FakeClaude {
        fn run(&self, args: &[&str]) -> Result<CliOutput, String> {
            let joined = args.join(" ");
            self.calls.borrow_mut().push(joined.clone());
            self.answers
                .iter()
                .find(|(prefix, _)| joined.starts_with(prefix))
                .map(|(_, a)| a.clone())
                .unwrap_or_else(|| Err(format!("unexpected call: {joined}")))
        }
    }

    fn ok(stdout: &str) -> Result<CliOutput, String> {
        Ok(CliOutput {
            success: true,
            stdout: stdout.to_string(),
            stderr: String::new(),
        })
    }

    fn fail(stdout: &str) -> Result<CliOutput, String> {
        Ok(CliOutput {
            success: false,
            stdout: stdout.to_string(),
            stderr: String::new(),
        })
    }

    fn spec(
        plugins: serde_json::Value,
        marketplaces: Option<serde_json::Value>,
    ) -> ClaudeSettingsSpec {
        let mut keys = vec!["enabledPlugins".to_string()];
        let mut values = serde_json::Map::new();
        values.insert("enabledPlugins".into(), plugins);
        if let Some(m) = marketplaces {
            keys.push("extraKnownMarketplaces".to_string());
            values.insert("extraKnownMarketplaces".into(), m);
        }
        ClaudeSettingsSpec {
            managed_keys: keys,
            values,
        }
    }

    fn gatewarden() -> serde_json::Value {
        serde_json::json!({"gatewarden-nexus": {"source": {
            "source": "github", "repo": "gwnexus/nexus-runtime-plugins", "ref": "nexus-core--v1.1.1"
        }}})
    }

    fn ws() -> PathBuf {
        std::env::temp_dir()
    }

    #[test]
    fn test_managed_plugins_only_true_and_managed() {
        let s = spec(
            serde_json::json!({"b@m": true, "a@m": true, "c@m": false}),
            None,
        );
        assert_eq!(managed_plugins(Some(&s)), vec!["a@m", "b@m"]);
        let unmanaged = ClaudeSettingsSpec {
            managed_keys: vec!["statusLine".into()],
            values: s.values.clone(),
        };
        assert!(managed_plugins(Some(&unmanaged)).is_empty());
        assert!(managed_plugins(None).is_empty());
    }

    #[test]
    fn test_marketplace_source_arg_shapes() {
        assert_eq!(
            marketplace_source_arg(&gatewarden()["gatewarden-nexus"]).as_deref(),
            Some("gwnexus/nexus-runtime-plugins#nexus-core--v1.1.1")
        );
        let no_ref = serde_json::json!({"source": {"source": "github", "repo": "o/r"}});
        assert_eq!(marketplace_source_arg(&no_ref).as_deref(), Some("o/r"));
        let git =
            serde_json::json!({"source": {"source": "git", "url": "https://x/y.git", "ref": "v1"}});
        assert_eq!(
            marketplace_source_arg(&git).as_deref(),
            Some("https://x/y.git#v1")
        );
        let dir = serde_json::json!({"source": {"source": "directory", "path": "./mkt"}});
        assert_eq!(marketplace_source_arg(&dir).as_deref(), Some("./mkt"));
        let odd = serde_json::json!({"source": {"source": "npm", "package": "x"}});
        assert_eq!(marketplace_source_arg(&odd), None);
    }

    #[test]
    fn test_is_present_respects_scope_and_project_path() {
        let here = ws();
        let installed = vec![
            InstalledPlugin {
                id: "user@m".into(),
                scope: Some("user".into()),
                project_path: None,
            },
            InstalledPlugin {
                id: "here@m".into(),
                scope: Some("project".into()),
                project_path: Some(here.display().to_string()),
            },
            InstalledPlugin {
                id: "elsewhere@m".into(),
                scope: Some("project".into()),
                project_path: Some("/definitely/not/this/workspace".into()),
            },
            InstalledPlugin {
                id: "legacy".into(),
                scope: None,
                project_path: None,
            },
        ];
        assert!(is_present(&installed, "user@m", &here));
        assert!(is_present(&installed, "here@m", &here));
        assert!(!is_present(&installed, "elsewhere@m", &here));
        assert!(is_present(&installed, "legacy@m", &here));
        assert!(!is_present(&installed, "absent@m", &here));
        assert_eq!(
            present_scope(&installed, "here@m", &here).as_deref(),
            Some("project")
        );
        assert_eq!(present_scope(&installed, "elsewhere@m", &here), None);
    }

    #[test]
    fn test_parse_installed_reads_scope_and_project_path() {
        let v = serde_json::json!([
            {"id": "a@m", "scope": "project", "projectPath": "/p"},
            {"name": "b", "marketplace": "m"}
        ]);
        let parsed = parse_installed(&v).unwrap();
        assert_eq!(parsed[0].scope.as_deref(), Some("project"));
        assert_eq!(parsed[0].project_path.as_deref(), Some("/p"));
        assert_eq!(parsed[1].id, "b@m");
        assert!(parse_installed(&serde_json::json!("x")).is_none());
    }

    #[test]
    fn test_sync_installs_missing_and_adds_declared_marketplace() {
        let cli = FakeClaude::new(vec![
            ("plugin list --json", ok(r#"[{"id":"present@claude-plugins-official","scope":"user"}]"#)),
            ("plugin marketplace list --json", ok(r#"[{"name":"claude-plugins-official"}]"#)),
            ("plugin marketplace update claude-plugins-official", ok("")),
            ("plugin marketplace add gwnexus/nexus-runtime-plugins#nexus-core--v1.1.1 --scope project --json",
                ok(r#"{"command":"marketplace-add","outcome":"ok"}"#)),
            ("plugin install", ok(r#"{"command":"install","outcome":"ok","message":"Successfully installed"}"#)),
        ]);
        let s = spec(
            serde_json::json!({
                "nexus-core@gatewarden-nexus": true,
                "frontend-design@claude-plugins-official": true,
                "present@claude-plugins-official": true
            }),
            Some(gatewarden()),
        );
        let (report, provenance) = sync(&cli, &ws(), Some(&s), &BTreeMap::new());

        assert_eq!(
            report.marketplaces,
            vec![
                MarketplaceOutcome {
                    name: "claude-plugins-official".into(),
                    state: MarketplaceState::Updated
                },
                MarketplaceOutcome {
                    name: "gatewarden-nexus".into(),
                    state: MarketplaceState::Added
                },
            ]
        );
        let states: Vec<(&str, &PluginState)> = report
            .plugins
            .iter()
            .map(|p| (p.id.as_str(), &p.state))
            .collect();
        assert_eq!(
            states,
            vec![
                (
                    "frontend-design@claude-plugins-official",
                    &PluginState::Installed
                ),
                ("nexus-core@gatewarden-nexus", &PluginState::Installed),
                ("present@claude-plugins-official", &PluginState::Present),
            ]
        );
        assert_eq!(
            provenance.keys().cloned().collect::<Vec<_>>(),
            vec![
                "frontend-design@claude-plugins-official",
                "nexus-core@gatewarden-nexus"
            ]
        );
        let calls = cli.calls();
        assert!(calls.contains(
            &"plugin install nexus-core@gatewarden-nexus --scope project --json".to_string()
        ));
        // Never auto-confirm a marketplace-declared command.
        assert!(calls.iter().all(|c| !c.contains("--yes")
            && !c.contains("-y ")
            && !c.contains("--accept-command")));
    }

    #[test]
    fn test_sync_all_present_makes_no_install_calls() {
        let cli = FakeClaude::new(vec![(
            "plugin list --json",
            ok(r#"[{"id":"a@m","scope":"user"}]"#),
        )]);
        let s = spec(serde_json::json!({"a@m": true}), None);
        let (report, provenance) = sync(&cli, &ws(), Some(&s), &BTreeMap::new());
        assert_eq!(report.plugins[0].state, PluginState::Present);
        assert!(report.marketplaces.is_empty());
        assert!(provenance.is_empty());
        assert_eq!(cli.calls(), vec!["plugin list --json"]);
    }

    #[test]
    fn test_sync_failure_is_reported_not_fatal() {
        let cli = FakeClaude::new(vec![
            ("plugin list --json", ok("[]")),
            ("plugin marketplace list --json", ok(r#"[{"name":"m"}]"#)),
            ("plugin marketplace update m", ok("")),
            (
                "plugin install bad@m",
                fail(
                    r#"{"command":"install","outcome":"failed","message":"Plugin \"bad\" not found in marketplace \"m\".","failureCode":"not_found"}"#,
                ),
            ),
            (
                "plugin install cmd@m",
                fail(
                    r#"{"command":"install","outcome":"failed","message":"needs confirmation","shownCommand":{"sha256":"abc"}}"#,
                ),
            ),
            (
                "plugin install good@m",
                ok(
                    r#"{"outcome":"ok","message":"Plugin \"good@m\" is already installed (scope: project)"}"#,
                ),
            ),
        ]);
        let s = spec(
            serde_json::json!({"bad@m": true, "cmd@m": true, "good@m": true}),
            None,
        );
        let (report, provenance) = sync(&cli, &ws(), Some(&s), &BTreeMap::new());
        assert_eq!(
            report.plugins[0].state,
            PluginState::Failed("Plugin \"bad\" not found in marketplace \"m\".".into())
        );
        assert!(
            matches!(&report.plugins[1].state, PluginState::Failed(r) if r.contains("needs confirmation") || r.contains("interactively"))
        );
        assert_eq!(report.plugins[2].state, PluginState::Present);
        assert!(provenance.is_empty());
    }

    #[test]
    fn test_sync_failed_marketplace_skips_its_plugins() {
        let cli = FakeClaude::new(vec![
            ("plugin list --json", ok("[]")),
            ("plugin marketplace list --json", ok("[]")),
            (
                "plugin marketplace add",
                fail(r#"{"outcome":"failed","message":"Permission denied (publickey)"}"#),
            ),
        ]);
        let s = spec(
            serde_json::json!({"nexus-core@gatewarden-nexus": true}),
            Some(gatewarden()),
        );
        let (report, _) = sync(&cli, &ws(), Some(&s), &BTreeMap::new());
        assert!(
            matches!(&report.marketplaces[0].state, MarketplaceState::Failed(r) if r.contains("publickey"))
        );
        assert!(
            matches!(&report.plugins[0].state, PluginState::Failed(r) if r.starts_with("marketplace unavailable"))
        );
        assert!(cli.calls().iter().all(|c| !c.starts_with("plugin install")));
    }

    #[test]
    fn test_sync_without_claude_reports_every_plugin() {
        let cli = FakeClaude::new(vec![(
            "plugin list --json",
            Err("could not start claude".into()),
        )]);
        let s = spec(serde_json::json!({"a@m": true}), None);
        let (report, _) = sync(&cli, &ws(), Some(&s), &BTreeMap::new());
        assert!(
            matches!(&report.plugins[0].state, PluginState::Failed(r) if r.contains("unavailable"))
        );
    }

    #[test]
    fn test_sync_releases_dropped_nexus_plugins_only() {
        let cli = FakeClaude::new(vec![(
            "plugin list --json",
            ok(r#"[{"id":"kept@m","scope":"user"}]"#),
        )]);
        let entry = CcxLockPluginEntry {
            scope: "project".into(),
            installed_at: "2026-10-05T00:00:00Z".into(),
        };
        let previous = BTreeMap::from([
            ("kept@m".to_string(), entry.clone()),
            ("dropped@m".to_string(), entry.clone()),
        ]);
        let s = spec(serde_json::json!({"kept@m": true}), None);
        let (report, provenance) = sync(&cli, &ws(), Some(&s), &previous);
        assert_eq!(report.released, vec!["dropped@m"]);
        assert_eq!(
            provenance.keys().cloned().collect::<Vec<_>>(),
            vec!["kept@m"]
        );
        // Nothing is uninstalled or disabled through the CLI.
        assert!(cli
            .calls()
            .iter()
            .all(|c| !c.contains("uninstall") && !c.contains("disable")));
    }

    #[test]
    fn test_sync_no_managed_plugins_makes_no_calls() {
        let cli = FakeClaude::new(vec![]);
        let (report, _) = sync(&cli, &ws(), None, &BTreeMap::new());
        assert_eq!(report, SyncReport::default());
        assert!(cli.calls().is_empty());
    }

    #[test]
    fn test_failure_reason_falls_back_to_stderr_and_truncates() {
        let out = CliOutput {
            success: false,
            stdout: String::new(),
            stderr: format!("✘ {}\nmore", "x".repeat(300)),
        };
        let reason = failure_reason(&out);
        assert!(reason.starts_with("xxx"));
        assert!(reason.ends_with('…'));
    }
}
