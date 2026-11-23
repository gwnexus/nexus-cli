//! Plugin environment and developer tool checks for `nexus preflight` and
//! the `nexus run` pre-launch check.
//!
//! The hub's MCP preflight (`GET /api/mcp/projects/{id}/preflight`) lists the
//! variables and settings the project's plugins need. It is called without
//! `env_keys`: no local variable name leaves the machine. Which variables are
//! set is decided here, against the process environment and the env files
//! `nexus run` exports (`.nexus/env`, `.env.nexus.local`; untracked only).
//!
//! The optional Sentry live check sends the local `SENTRY_ACCESS_TOKEN` to
//! the Sentry API, never to the Nexus API, and only to `sentry.io`,
//! `*.sentry.io` or a host the operator lists in `NEXUS_TRUSTED_SENTRY_HOSTS`.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use nexus_core::api::{McpPreflightResponse, NexusClient, SentryDevTool};

use super::preflight::CheckResult;

/// Environment variable listing extra Sentry hosts the operator trusts with
/// the Sentry token, comma-separated: exact `host[:port]` or `*.domain`.
pub(crate) const TRUSTED_SENTRY_HOSTS_ENV: &str = "NEXUS_TRUSTED_SENTRY_HOSTS";

/// Timeout of the Sentry live check.
const LIVE_TIMEOUT: Duration = Duration::from_secs(5);

/// Hub-supplied text as one safe terminal line: no control or escape
/// characters, at most `max` characters.
fn clean(text: &str, max: usize) -> String {
    console::truncate_str(&super::dispatch_listen::flatten(text), max, "…").to_string()
}

/// Values of the env files `nexus run` exports, without git-tracked files.
fn env_file_values(workspace: &Path) -> HashMap<String, String> {
    let mut values = HashMap::new();
    for rel in [".nexus/env", ".env.nexus.local"] {
        let path = workspace.join(rel);
        if !path.is_file() || nexus_core::config::trust::is_git_tracked(&path) {
            continue;
        }
        values.extend(super::run::parse_env_file(&path));
    }
    values
}

/// The local value of `name`: the process environment first (it wins in
/// `nexus run` too), then the env files. Empty values count as unset.
fn local_value(name: &str, files: &HashMap<String, String>) -> Option<String> {
    std::env::var(name)
        .ok()
        .or_else(|| files.get(name).cloned())
        .filter(|v| !v.is_empty())
}

/// One check line per plugin variable and setting, decided locally. `set`
/// tells whether a variable has a local value. Hubs before 0.20.0 report no
/// checks: one WARN line.
pub(crate) fn evaluate(
    resp: &McpPreflightResponse,
    set: impl Fn(&str) -> bool,
) -> Vec<CheckResult> {
    let Some(checks) = &resp.checks else {
        return vec![CheckResult::Warn(
            "hub does not report plugin checks (needs hub 0.20.0)".into(),
        )];
    };
    let sentry = resp.dev_tools.as_ref().and_then(|d| d.sentry.as_ref());
    let mut out = Vec::new();
    for c in checks {
        let what = format!("{} ({})", c.env_var, c.plugin);
        if set(&c.env_var) {
            out.push(CheckResult::Pass(format!("{what} set")));
            continue;
        }
        let scopes = sentry
            .filter(|s| s.token_env == c.env_var && !s.required_scopes.is_empty())
            .map(|s| format!("; token scopes: {}", s.required_scopes.join(", ")))
            .unwrap_or_default();
        let msg = format!("{what} not set{scopes} -- add it to .env.nexus.local");
        out.push(if c.required {
            CheckResult::Fail(msg)
        } else {
            CheckResult::Warn(format!("{msg} (optional)"))
        });
    }
    for c in resp.config_checks.iter().flatten() {
        let what = format!("{} ({})", c.setting, c.plugin);
        match c.status.as_str() {
            "ok" => out.push(CheckResult::Pass(format!("{what} configured"))),
            "invalid" => out.push(CheckResult::Fail(format!(
                "{what} invalid{} -- fix it in the Hub",
                c.message
                    .as_deref()
                    .map(|m| format!(": {}", clean(m, 160)))
                    .unwrap_or_default()
            ))),
            _ => out.push(CheckResult::Fail(format!(
                "{what} missing -- set it in the Hub"
            ))),
        }
    }
    if out.is_empty() {
        out.push(CheckResult::Pass("no plugin variables required".into()));
    }
    out
}

/// Whether the Sentry token may be sent to `host` (`host[:port]`, no
/// scheme): `sentry.io` and `*.sentry.io` without a port, or an entry of
/// `trusted` (exact `host[:port]` or `*.domain`).
pub(crate) fn sentry_host_trusted(host: &str, trusted: Option<&str>) -> bool {
    let host = host.trim().to_ascii_lowercase();
    if host.is_empty() {
        return false;
    }
    if host == "sentry.io" || host.ends_with(".sentry.io") && !host.contains(':') {
        return true;
    }
    trusted
        .unwrap_or_default()
        .split(',')
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .any(|e| match e.strip_prefix("*.") {
            Some(domain) => host.ends_with(&format!(".{domain}")) && !host.contains(':'),
            None => e == host,
        })
}

/// `https://{host}/api/0/organizations/{org}/`, when `host` is a plain
/// `host[:port]` and `org` a plain slug (nothing that could change the URL's
/// host or path).
pub(crate) fn sentry_org_url(host: &str, org: &str) -> Option<reqwest::Url> {
    let host_ok = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'));
    let org_ok = !org.is_empty()
        && org
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'));
    if !host_ok || !org_ok {
        return None;
    }
    let url = reqwest::Url::parse(&format!("https://{host}/api/0/organizations/{org}/")).ok()?;
    let authority = match url.port() {
        Some(p) => format!("{}:{p}", url.host_str()?),
        None => url.host_str()?.to_string(),
    };
    (authority.eq_ignore_ascii_case(host)).then_some(url)
}

/// Map the Sentry API's answer to a check line.
fn live_result(org: &str, status: Result<u16, String>) -> CheckResult {
    match status {
        Ok(200) => CheckResult::Pass(format!("Sentry organization '{org}' reachable with the token")),
        Ok(401) | Ok(403) => CheckResult::Fail(format!(
            "Sentry rejected the token for '{org}' (HTTP 401/403): token invalid or missing org:read"
        )),
        Ok(404) => CheckResult::Fail(format!("Sentry organization '{org}' not found (HTTP 404)")),
        Ok(code) => CheckResult::Warn(format!("Sentry answered HTTP {code}")),
        Err(e) => CheckResult::Warn(format!("Sentry not reachable: {e}")),
    }
}

/// GET `url` with the token as bearer auth: no redirects, short timeout.
/// Returns the HTTP status or a short error (never containing the token).
async fn probe(url: reqwest::Url, token: &str) -> Result<u16, String> {
    let client = reqwest::Client::builder()
        .timeout(LIVE_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| "could not build an HTTP client".to_string())?;
    match client.get(url).bearer_auth(token).send().await {
        Ok(resp) => Ok(resp.status().as_u16()),
        Err(e) if e.is_timeout() => Err("timeout".into()),
        Err(e) if e.is_connect() => Err("connection failed".into()),
        Err(_) => Err("request failed".into()),
    }
}

/// The Sentry live check, or `None` when the project has no Sentry
/// developer tool.
pub(crate) async fn sentry_live(
    sentry: Option<&SentryDevTool>,
    files: &HashMap<String, String>,
) -> Option<CheckResult> {
    let s = sentry?;
    let Some(org) = s.organization_slug.as_deref().filter(|o| !o.is_empty()) else {
        return Some(CheckResult::Warn(
            "Sentry live check skipped: no organization set in the Hub".into(),
        ));
    };
    let Some(token) = local_value(&s.token_env, files) else {
        return Some(CheckResult::Warn(format!(
            "Sentry live check skipped: {} not set",
            s.token_env
        )));
    };
    let trusted = std::env::var(TRUSTED_SENTRY_HOSTS_ENV).ok();
    if !sentry_host_trusted(&s.host, trusted.as_deref()) {
        return Some(CheckResult::Warn(format!(
            "Sentry live check skipped: host '{}' not trusted locally (add it to {})",
            clean(&s.host, 80),
            TRUSTED_SENTRY_HOSTS_ENV
        )));
    }
    let Some(url) = sentry_org_url(&s.host, org) else {
        return Some(CheckResult::Warn(
            "Sentry live check skipped: host or organization is not a plain name".into(),
        ));
    };
    Some(live_result(org, probe(url, &token).await))
}

/// Everything `nexus preflight` shows for plugins: the variable and setting
/// lines, then (with `live`) the Sentry live check.
pub(crate) async fn checks(
    api_url: &str,
    token: Option<&str>,
    project_id: Option<&str>,
    workspace: &Path,
    live: bool,
) -> Vec<(&'static str, CheckResult)> {
    let (Some(token), Some(project_id)) = (token, project_id) else {
        return vec![(
            "plugin-env",
            CheckResult::Warn("skipped (needs login and a linked project)".into()),
        )];
    };
    let resp = match NexusClient::new(api_url, Some(token.to_string())) {
        Ok(client) => client.mcp_preflight(project_id).await,
        Err(e) => {
            return vec![(
                "plugin-env",
                CheckResult::Fail(format!("client error: {e}")),
            )];
        }
    };
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            return vec![(
                "plugin-env",
                CheckResult::Warn(format!("hub preflight failed: {e}")),
            )];
        }
    };
    let files = env_file_values(workspace);
    let mut out: Vec<(&'static str, CheckResult)> =
        evaluate(&resp, |name| local_value(name, &files).is_some())
            .into_iter()
            .map(|c| ("plugin-env", c))
            .collect();
    if live {
        let sentry = resp.dev_tools.as_ref().and_then(|d| d.sentry.as_ref());
        if let Some(c) = sentry_live(sentry, &files).await {
            out.push(("sentry-live", c));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::api::{DevTools, PluginConfigCheck, PluginEnvCheck};

    fn resp(checks: Option<Vec<PluginEnvCheck>>) -> McpPreflightResponse {
        McpPreflightResponse {
            project_id: "p".into(),
            plugins: vec![],
            headroom_enabled: false,
            checks,
            config_checks: None,
            all_required_ok: None,
            dev_tools: None,
        }
    }

    fn check(var: &str, required: bool) -> PluginEnvCheck {
        PluginEnvCheck {
            plugin: "sentry".into(),
            env_var: var.into(),
            required,
            description: None,
            status: Some("missing".into()),
        }
    }

    fn sentry(host: &str) -> SentryDevTool {
        SentryDevTool {
            organization_slug: Some("acme".into()),
            project_slug: None,
            host: host.into(),
            token_env: "SENTRY_ACCESS_TOKEN".into(),
            required_scopes: vec!["org:read".into(), "event:read".into()],
        }
    }

    #[test]
    fn test_older_hub_without_checks_warns() {
        let out = evaluate(&resp(None), |_| true);
        assert_eq!(out.len(), 1);
        assert!(out[0].is_warn());
        assert!(out[0].message().contains("does not report plugin checks"));
    }

    #[test]
    fn test_project_without_plugin_variables_passes() {
        let out = evaluate(&resp(Some(vec![])), |_| false);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], CheckResult::Pass(_)));
    }

    #[test]
    fn test_missing_required_token_fails_with_name_and_scopes() {
        let mut r = resp(Some(vec![
            check("SENTRY_ACCESS_TOKEN", true),
            check("SENTRY_X", false),
        ]));
        r.dev_tools = Some(DevTools {
            sentry: Some(sentry("sentry.io")),
        });
        let out = evaluate(&r, |_| false);
        assert!(out[0].is_fail());
        assert!(out[0].message().contains("SENTRY_ACCESS_TOKEN"));
        assert!(out[0].message().contains("org:read, event:read"));
        assert!(out[1].is_warn());
        // Decided locally: the hub's "missing" does not count once set.
        let out = evaluate(&r, |n| n == "SENTRY_ACCESS_TOKEN");
        assert!(matches!(out[0], CheckResult::Pass(_)));
    }

    #[test]
    fn test_config_checks_missing_and_invalid_fail() {
        let mut r = resp(Some(vec![]));
        r.config_checks = Some(vec![
            PluginConfigCheck {
                plugin: "sentry".into(),
                setting: "sentry_config.organization_slug".into(),
                status: "missing".into(),
                message: None,
            },
            PluginConfigCheck {
                plugin: "sentry".into(),
                setting: "sentry_config".into(),
                status: "invalid".into(),
                message: Some("host \u{1b}[31mx is not allowed".into()),
            },
            PluginConfigCheck {
                plugin: "sentry".into(),
                setting: "other".into(),
                status: "ok".into(),
                message: None,
            },
        ]);
        let out = evaluate(&r, |_| false);
        assert!(out[0].is_fail());
        assert!(out[1].is_fail());
        assert!(out[1].message().contains("is not allowed"));
        assert!(!out[1].message().contains('\u{1b}'));
        assert!(matches!(out[2], CheckResult::Pass(_)));
    }

    #[test]
    fn test_output_never_contains_values() {
        let files: HashMap<String, String> = [(
            "SENTRY_ACCESS_TOKEN".to_string(),
            "sntrys_secret_value".to_string(),
        )]
        .into();
        let r = resp(Some(vec![check("SENTRY_ACCESS_TOKEN", true)]));
        let out = evaluate(&r, |n| local_value(n, &files).is_some());
        assert!(out
            .iter()
            .all(|c| !c.message().contains("sntrys_secret_value")));
    }

    #[test]
    fn test_sentry_host_trust() {
        assert!(sentry_host_trusted("sentry.io", None));
        assert!(sentry_host_trusted("de.sentry.io", None));
        assert!(sentry_host_trusted("DE.Sentry.IO", None));
        assert!(!sentry_host_trusted("de.sentry.io:8443", None));
        assert!(!sentry_host_trusted("evilsentry.io", None));
        assert!(!sentry_host_trusted("sentry.io.evil.com", None));
        assert!(!sentry_host_trusted("sentry.acme.com", None));
        assert!(!sentry_host_trusted("", None));
        let trusted = Some("sentry.acme.com, *.internal.example:9000,*.corp.example");
        assert!(sentry_host_trusted("sentry.acme.com", trusted));
        assert!(sentry_host_trusted("a.corp.example", trusted));
        assert!(!sentry_host_trusted("corp.example", trusted));
        assert!(!sentry_host_trusted("a.corp.example:8443", trusted));
        assert!(!sentry_host_trusted("other.example", trusted));
    }

    #[test]
    fn test_sentry_org_url_refuses_anything_but_plain_names() {
        assert_eq!(
            sentry_org_url("de.sentry.io", "acme").unwrap().as_str(),
            "https://de.sentry.io/api/0/organizations/acme/"
        );
        assert!(sentry_org_url("sentry.acme.com:8443", "acme").is_some());
        for (host, org) in [
            ("evil.example/x", "acme"),
            ("evil.example\\.sentry.io", "acme"),
            ("evil.example?.sentry.io", "acme"),
            ("user@sentry.io", "acme"),
            ("sentry.io", "../../x"),
            ("sentry.io", "a/b"),
            ("sentry.io", ""),
            ("", "acme"),
        ] {
            assert!(sentry_org_url(host, org).is_none(), "{host} {org}");
        }
    }

    #[test]
    fn test_live_result_mapping() {
        assert!(matches!(live_result("acme", Ok(200)), CheckResult::Pass(_)));
        assert!(live_result("acme", Ok(401)).is_fail());
        assert!(live_result("acme", Ok(403)).message().contains("org:read"));
        assert!(live_result("acme", Ok(500)).is_warn());
        assert!(live_result("acme", Err("timeout".into())).is_warn());
    }

    #[tokio::test]
    async fn test_untrusted_host_never_receives_the_token() {
        // A listener that records whether anything connects. The host is
        // not trusted, so the check must stop before any request.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let host = format!("127.0.0.1:{}", listener.local_addr().unwrap().port());
        let files: HashMap<String, String> =
            [("SENTRY_ACCESS_TOKEN".to_string(), "tok".to_string())].into();
        let out = sentry_live(Some(&sentry(&host)), &files).await.unwrap();
        assert!(out.is_warn());
        assert!(out.message().contains("not trusted locally"));
        assert!(
            listener.accept().is_err(),
            "the untrusted host was contacted"
        );
    }

    #[tokio::test]
    async fn test_live_check_skips_without_token_or_org() {
        let files = HashMap::new();
        let mut s = sentry("sentry.io");
        s.token_env = "NEXUS_TEST_SENTRY_TOKEN_UNSET_42".into();
        let out = sentry_live(Some(&s), &files).await.unwrap();
        assert!(out.message().contains("not set"));
        s.organization_slug = None;
        let out = sentry_live(Some(&s), &files).await.unwrap();
        assert!(out.message().contains("no organization"));
        assert!(sentry_live(None, &files).await.is_none());
    }
}
