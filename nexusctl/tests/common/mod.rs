//! Functional-test harness: runs the real `nexus` binary in a temporary git
//! workspace against a local mock of the Nexus API.
//!
//! The mock answers JSON per `METHOD /path` (query string ignored) and
//! records every request, so tests can assert what was sent. Unknown routes
//! answer 404. The binary gets its own `HOME`, so no real configuration,
//! credentials or caches are read or written.

#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

/// A synthetic project id (never a real one).
pub const PROJECT_ID: &str = "00000000-0000-4000-8000-000000000001";

/// One recorded request.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub body: String,
}

type Routes = Arc<Mutex<HashMap<String, (u16, String)>>>;

/// A minimal HTTP/1.1 server on 127.0.0.1 (one connection at a time,
/// `Connection: close`).
pub struct MockApi {
    pub url: String,
    routes: Routes,
    requests: Arc<Mutex<Vec<Request>>>,
}

impl MockApi {
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let routes: Routes = Arc::default();
        let requests: Arc<Mutex<Vec<Request>>> = Arc::default();
        let (r, q) = (routes.clone(), requests.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() {
                    continue;
                }
                let mut parts = line.split_whitespace();
                let method = parts.next().unwrap_or_default().to_string();
                let target = parts.next().unwrap_or_default().to_string();
                let path = target.split('?').next().unwrap_or_default().to_string();
                let mut len = 0usize;
                loop {
                    let mut h = String::new();
                    if reader.read_line(&mut h).is_err() || h == "\r\n" || h.is_empty() {
                        break;
                    }
                    if let Some((k, v)) = h.split_once(':') {
                        if k.eq_ignore_ascii_case("content-length") {
                            len = v.trim().parse().unwrap_or(0);
                        }
                    }
                }
                let mut body = vec![0u8; len];
                let _ = reader.read_exact(&mut body);
                q.lock().unwrap().push(Request {
                    method: method.clone(),
                    path: path.clone(),
                    body: String::from_utf8_lossy(&body).into_owned(),
                });
                // `POST /path#action` (the JSON body's "action") first,
                // then `METHOD /path`.
                let action = serde_json::from_slice::<serde_json::Value>(&body)
                    .ok()
                    .and_then(|v| v.get("action").and_then(|a| a.as_str()).map(String::from));
                let (status, payload) = {
                    let routes = r.lock().unwrap();
                    action
                        .and_then(|a| routes.get(&format!("{method} {path}#{a}")).cloned())
                        .or_else(|| routes.get(&format!("{method} {path}")).cloned())
                        .unwrap_or((404, r#"{"error":"not found"}"#.to_string()))
                };
                let reason = if status < 400 { "OK" } else { "Error" };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
            }
        });
        Self {
            url,
            routes,
            requests,
        }
    }

    /// Answer `METHOD /path` (or `METHOD /path#action`) with `status` and
    /// the JSON `body`.
    pub fn route(&self, method: &str, path: &str, status: u16, body: serde_json::Value) {
        self.routes
            .lock()
            .unwrap()
            .insert(format!("{method} {path}"), (status, body.to_string()));
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    /// Requests other than GET (what the run tried to change on the server).
    pub fn writes(&self) -> Vec<Request> {
        self.requests()
            .into_iter()
            .filter(|r| r.method != "GET")
            .collect()
    }

    /// The bodies of the workspace pushes (`ws_push`) sent so far.
    pub fn pushes(&self) -> Vec<serde_json::Value> {
        self.requests()
            .into_iter()
            .filter_map(|r| serde_json::from_str::<serde_json::Value>(&r.body).ok())
            .filter(|v| v["action"] == "ws_push")
            .collect()
    }

    pub fn clear_requests(&self) {
        self.requests.lock().unwrap().clear();
    }
}

/// A temporary git workspace linked to [`PROJECT_ID`], with its own `HOME`.
pub struct Workspace {
    _root: tempfile::TempDir,
    pub dir: PathBuf,
    pub home: PathBuf,
}

impl Workspace {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("ws");
        let home = root.path().join("home");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(home.join(".config/nexus")).unwrap();
        std::fs::write(
            home.join(".config/nexus/config.toml"),
            "check_updates = false\n",
        )
        .unwrap();
        let ws = Self {
            _root: root,
            dir,
            home,
        };
        ws.git(&["init", "-q", "-b", "main"]);
        ws.git(&["config", "user.email", "test@example.invalid"]);
        ws.git(&["config", "user.name", "Test"]);
        ws.write(
            ".nexus/config.toml",
            &format!("[project]\nname = \"demo\"\nid = \"{PROJECT_ID}\"\n"),
        );
        ws
    }

    /// A git repository with its own `HOME` that is not yet linked to a
    /// project (no `.nexus/`), for `nexus init`.
    pub fn unlinked() -> Self {
        let ws = Self::new();
        std::fs::remove_dir_all(ws.path(".nexus")).unwrap();
        ws
    }

    pub fn path(&self, rel: &str) -> PathBuf {
        self.dir.join(rel)
    }

    pub fn write(&self, rel: &str, content: &str) {
        let p = self.path(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.path(rel)).unwrap_or_else(|e| panic!("cannot read {rel}: {e}"))
    }

    pub fn exists(&self, rel: &str) -> bool {
        self.path(rel).symlink_metadata().is_ok()
    }

    pub fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.dir)
            .env("HOME", &self.home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Run `nexus <args>` against `api` without a terminal.
    pub fn nexus(&self, api: &MockApi, args: &[&str]) -> Run {
        let out = Command::new(env!("CARGO_BIN_EXE_nexus"))
            .args(args)
            .current_dir(&self.dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("NEXUS_API_URL", &api.url)
            .env("NEXUS_PRIVATE_TOKEN", "test-token")
            .env("NEXUS_NO_CODE_SIGNALS", "1")
            .env("NEXUS_SKIP_PLUGIN_SYNC", "1")
            .env("NO_COLOR", "1")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        Run::from(out)
    }
}

/// The outcome of one `nexus` run.
pub struct Run {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

impl From<Output> for Run {
    fn from(o: Output) -> Self {
        Self {
            ok: o.status.success(),
            stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
        }
    }
}

impl Run {
    pub fn all(&self) -> String {
        format!("{}\n{}", self.stdout, self.stderr)
    }

    #[track_caller]
    pub fn assert_ok(&self) -> &Self {
        assert!(self.ok, "nexus failed:\n{}", self.all());
        self
    }

    #[track_caller]
    pub fn assert_contains(&self, needle: &str) -> &Self {
        assert!(
            self.all().contains(needle),
            "expected {needle:?} in output:\n{}",
            self.all()
        );
        self
    }
}

/// The sha256 hex of `s`, as Nexus records it.
pub fn sha256(s: &str) -> String {
    use sha2::Digest;
    sha2::Sha256::digest(s.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn path_str(p: &Path) -> &str {
    p.to_str().unwrap()
}

/// What the fake backend serves for [`PROJECT_ID`]; `install` turns it into
/// routes. Change a field and install again to model a new backend state.
#[derive(Clone)]
pub struct Backend {
    pub owner: &'static str,
    /// (target_path, body)
    pub agent_files: Vec<(String, String)>,
    /// (name, body)
    pub skills: Vec<(String, String)>,
    /// (title, category, level)
    pub directives: Vec<(String, String, String)>,
    /// (title, status)
    pub tasks: Vec<(String, String)>,
    /// `None`: no workspace fork (404).
    pub devbox_json: Option<String>,
    /// (path, body, executable)
    pub scripts: Vec<(String, String, bool)>,
}

impl Default for Backend {
    fn default() -> Self {
        Self {
            owner: "opencode",
            agent_files: vec![],
            skills: vec![],
            directives: vec![],
            tasks: vec![],
            devbox_json: None,
            scripts: vec![],
        }
    }
}

impl Backend {
    pub fn install(&self, api: &MockApi) {
        use serde_json::json;
        let project = json!({"id": PROJECT_ID, "slug": "demo", "name": "demo"});
        api.route(
            "GET",
            "/api/mcp/identity",
            200,
            json!({"userId": "u-1", "email": "dev@example.invalid", "displayName": "Dev",
                   "isPlatformAdmin": false, "isPlatformOwner": false, "tenantId": "t-1"}),
        );
        let files: Vec<_> = self
            .agent_files
            .iter()
            .enumerate()
            .map(|(i, (path, body))| {
                json!({"file_key": format!("f{i}"), "target_path": path, "name": path,
                       "description": null, "category": "agent", "version": 1, "body": body})
            })
            .collect();
        api.route(
            "POST",
            "/api/mcp/agent-files#af_export",
            200,
            json!({"project_id": PROJECT_ID, "project_name": "demo", "agent_files": files,
                   "count": files.len(), "agentic_root": ".nexus", "agent_owner": self.owner}),
        );
        let skills: Vec<_> = self
            .skills
            .iter()
            .enumerate()
            .map(|(i, (name, body))| {
                json!({"skill_id": format!("s{i}"), "name": name, "description": "d",
                       "version": 1, "body": body, "command_slug": null})
            })
            .collect();
        api.route(
            "POST",
            "/api/mcp/skills#sk_export",
            200,
            json!({"action": "sk_export", "project": project, "skills": skills,
                   "count": skills.len()}),
        );
        let directives: Vec<_> = self
            .directives
            .iter()
            .enumerate()
            .map(|(i, (title, cat, level))| {
                json!({"id": format!("d{i}"), "title": title, "body": null, "category": cat,
                       "priority": "normal", "level": level})
            })
            .collect();
        api.route(
            "POST",
            "/api/mcp/directives#directive_export",
            200,
            json!({"action": "directive_export", "project": project, "directives": directives,
                   "count": directives.len()}),
        );
        let tasks: Vec<_> = self
            .tasks
            .iter()
            .enumerate()
            .map(|(i, (title, status))| {
                json!({"id": format!("t{i}"), "project_id": PROJECT_ID, "title": title,
                       "description": null, "status": status, "priority": "high",
                       "assignee": null, "created_by": null,
                       "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-02T00:00:00Z"})
            })
            .collect();
        api.route(
            "POST",
            "/api/mcp/tasks#task_list",
            200,
            json!({"action": "task_list", "project_id": PROJECT_ID, "count": tasks.len(),
                   "tasks": tasks}),
        );
        api.route(
            "GET",
            &format!("/api/mcp/projects/{PROJECT_ID}"),
            200,
            json!({"project": {"id": PROJECT_ID, "name": "demo", "slug": "demo",
                   "description": null, "status": "active",
                   "created_at": "2026-01-01T00:00:00Z", "agent_owner": self.owner,
                   "agentic_root": ".nexus"}}),
        );
        api.route(
            "POST",
            "/api/mcp/agent-files#ws_push",
            200,
            json!({"action": "ws_push", "project_id": PROJECT_ID, "fork_id": "fork-2",
                   "fork_name": "pushed", "version": 2, "previous_fork_id": "fork-1",
                   "previous_fork_name": "first", "files_pushed": []}),
        );
        match &self.devbox_json {
            Some(devbox) => {
                let scripts: Vec<_> = self
                    .scripts
                    .iter()
                    .map(|(p, b, x)| json!({"path": p, "body": b, "executable": x}))
                    .collect();
                api.route(
                    "POST",
                    "/api/mcp/agent-files#ws_export",
                    200,
                    json!({"devbox_json": devbox, "scripts": scripts,
                           "meta": {"fork_id": "fork-1", "version": 1,
                                    "scripts_path": "scripts/devbox"},
                           "project_id": PROJECT_ID, "fork_id": "fork-1"}),
                );
            }
            None => api.route(
                "POST",
                "/api/mcp/agent-files#ws_export",
                404,
                json!({"error": "no workspace fork"}),
            ),
        }
    }
}

/// Every file below the workspace (except `.git`) with its bytes, to prove
/// a run changed nothing.
pub fn snapshot(ws: &Workspace) -> std::collections::BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            let rel = p.strip_prefix(base).unwrap().to_string_lossy().into_owned();
            if rel == ".git" {
                continue;
            }
            let ft = e.file_type().unwrap();
            if ft.is_dir() {
                walk(base, &p, out);
            } else {
                out.insert(rel, std::fs::read(&p).unwrap_or_default());
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(&ws.dir, &ws.dir, &mut out);
    out
}

/// A backend with one of everything pull writes.
pub fn full_backend() -> Backend {
    Backend {
        agent_files: vec![(".nexus/AGENTS.md".into(), "# Agents\n".into())],
        skills: vec![("review".into(), "Review things.\n".into())],
        directives: vec![("Be precise".into(), "communication".into(), "must".into())],
        tasks: vec![("Fix the bug".into(), "open".into())],
        devbox_json: Some("{\n  \"packages\": []\n}\n".into()),
        scripts: vec![
            (
                "scripts/devbox/setup.sh".into(),
                "#!/bin/sh\necho setup\n".into(),
                true,
            ),
            (
                "scripts/devbox/check.sh".into(),
                "#!/bin/sh\necho check\n".into(),
                true,
            ),
        ],
        ..Default::default()
    }
}

/// A started mock serving `backend`, and a fresh workspace.
pub fn setup(backend: &Backend) -> (MockApi, Workspace) {
    let api = MockApi::start();
    backend.install(&api);
    (api, Workspace::new())
}
