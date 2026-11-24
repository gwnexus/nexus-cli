//! Functional tests for the commands that work on a pulled workspace:
//! `push`, `status`, `reset`, `stash` and `deinit`. The real binary runs
//! against a fake backend in a temporary git workspace.

mod common;
use common::*;

const SETUP: &str = "scripts/devbox/setup.sh";
const CHECK: &str = "scripts/devbox/check.sh";

fn pulled() -> (MockApi, Workspace) {
    let (api, ws) = setup(&full_backend());
    ws.nexus(&api, &["pull", "--trust"]).assert_ok();
    api.clear_requests();
    (api, ws)
}

/// Nothing changed locally: push sends nothing.
#[test]
fn push_without_changes_sends_nothing() {
    let (api, ws) = pulled();
    ws.nexus(&api, &["push"]).assert_ok();
    assert!(api.pushes().is_empty(), "pushed without local changes");
}

/// A local edit is pushed as a new fork that carries the edit and keeps
/// the untouched fork files.
#[test]
fn push_sends_the_edit_and_keeps_the_rest() {
    let (api, ws) = pulled();
    ws.write(SETUP, "#!/bin/sh\necho mine\n");
    ws.nexus(&api, &["push"]).assert_ok();
    let pushes = api.pushes();
    assert_eq!(pushes.len(), 1, "expected one push");
    // Script paths are relative to `scripts/devbox/`.
    let files = &pushes[0]["script_files"];
    assert_eq!(files["setup.sh"], "#!/bin/sh\necho mine\n");
    assert_eq!(
        files["check.sh"], "#!/bin/sh\necho check\n",
        "untouched file missing"
    );
    assert!(pushes[0]["devbox_json"].is_object(), "devbox.json missing");
}

/// `--dry-run` shows the plan and sends nothing.
#[test]
fn push_dry_run_sends_nothing() {
    let (api, ws) = pulled();
    ws.write(SETUP, "#!/bin/sh\necho mine\n");
    ws.nexus(&api, &["push", "--dry-run"]).assert_ok();
    assert!(api.pushes().is_empty());
}

/// A workspace file that cannot be read stops the push before anything is
/// sent, instead of publishing a fork without it.
#[cfg(unix)]
#[test]
fn push_stops_on_an_unreadable_file() {
    use std::os::unix::fs::PermissionsExt;
    if is_root() {
        return;
    }
    let (api, ws) = pulled();
    ws.write(SETUP, "#!/bin/sh\necho mine\n");
    std::fs::set_permissions(ws.path(CHECK), std::fs::Permissions::from_mode(0o000)).unwrap();
    let run = ws.nexus(&api, &["push"]);
    std::fs::set_permissions(ws.path(CHECK), std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(!run.ok, "push went on:\n{}", run.all());
    assert!(api.pushes().is_empty(), "an incomplete fork was pushed");
}

/// A sync manifest that cannot be parsed never leads to a push (without
/// records nothing is known to be a local edit) and is never reset.
#[test]
fn push_never_pushes_past_a_broken_sync_manifest() {
    let (api, ws) = pulled();
    ws.write(SETUP, "#!/bin/sh\necho mine\n");
    ws.write(".nexus/sync-manifest.json", "{ broken");
    let _ = ws.nexus(&api, &["push"]);
    assert!(api.pushes().is_empty());
    assert_eq!(ws.read(".nexus/sync-manifest.json"), "{ broken");
}

/// `status` tells a clean workspace (exit 0) from pending changes (exit
/// non-zero, the file listed) and does not write.
#[test]
fn status_reports_local_edits_without_writing() {
    let (api, ws) = pulled();
    ws.nexus(&api, &["status"])
        .assert_ok()
        .assert_contains("Workspace is clean");
    ws.write(SETUP, "#!/bin/sh\necho mine\n");
    let before = snapshot(&ws);
    let run = ws.nexus(&api, &["status"]);
    assert!(!run.ok, "pending change not signalled by the exit code");
    run.assert_contains(SETUP).assert_contains("MODIFIED");
    assert_eq!(snapshot(&ws), before, "status changed the workspace");
}

/// `reset` brings a locally edited file back to the backend version.
#[test]
fn reset_restores_the_backend_version() {
    let (api, ws) = pulled();
    ws.write(SETUP, "#!/bin/sh\necho mine\n");
    ws.nexus(&api, &["reset", SETUP, "-y"]).assert_ok();
    assert_eq!(ws.read(SETUP), "#!/bin/sh\necho setup\n");
    assert!(api.pushes().is_empty());
}

/// The documented flow: `stash` saves a local edit, `pull --force` takes
/// the backend version, `stash pop` brings the edit back.
#[test]
fn stash_round_trip() {
    let (api, ws) = pulled();
    ws.write(SETUP, "#!/bin/sh\necho mine\n");
    ws.nexus(&api, &["stash", "-y"]).assert_ok();
    let mut next = full_backend();
    next.scripts[0].1 = "#!/bin/sh\necho v2\n".into();
    next.install(&api);
    ws.nexus(&api, &["pull", "--trust", "--force"]).assert_ok();
    assert_eq!(ws.read(SETUP), "#!/bin/sh\necho v2\n");
    ws.nexus(&api, &["stash", "pop", "-y"]).assert_ok();
    assert_eq!(ws.read(SETUP), "#!/bin/sh\necho mine\n");
}

/// `deinit` removes what Nexus wrote and keeps what the operator wrote or
/// edited.
#[test]
fn deinit_removes_only_what_nexus_owns() {
    let (api, ws) = pulled();
    ws.write("README.md", "# my project\n");
    ws.write(".nexus/AGENTS.md", "# my edited agents\n");
    ws.write(".nexus/notes.md", "my notes\n");
    let mut doc: serde_json::Value = serde_json::from_str(&ws.read("opencode.json")).unwrap();
    doc["mcp"]["mine"] = serde_json::json!({"type": "local", "command": ["x"]});
    ws.write(
        "opencode.json",
        &serde_json::to_string_pretty(&doc).unwrap(),
    );

    ws.nexus(&api, &["deinit", "--force"]).assert_ok();

    assert_eq!(ws.read("README.md"), "# my project\n");
    assert_eq!(ws.read(".nexus/AGENTS.md"), "# my edited agents\n");
    let doc: serde_json::Value = serde_json::from_str(&ws.read("opencode.json")).unwrap();
    assert!(doc["mcp"]["mine"].is_object(), "operator server removed");
    assert!(doc["mcp"].get("nexus").is_none(), "Nexus server stayed");
    for gone in [
        ".nexus/skills/s0/SKILL.md",
        ".nexus/directives.md",
        ".nexus/TASKS.md",
        ".nexus/generated",
        ".nexus/sync-manifest.json",
    ] {
        assert!(!ws.exists(gone), "{gone} stayed");
    }
    assert!(ws.exists(".nexus/config.toml"), "project binding removed");
    assert!(ws.exists(".nexus/notes.md"), "operator file removed");
    // Nothing left to do: a second run changes nothing.
    let before = snapshot(&ws);
    ws.nexus(&api, &["deinit", "--force"]).assert_ok();
    assert_eq!(snapshot(&ws), before);
}

#[cfg(unix)]
fn is_root() -> bool {
    extern "C" {
        fn geteuid() -> u32;
    }
    // SAFETY: geteuid has no preconditions.
    unsafe { geteuid() == 0 }
}

/// While another command holds the workspace lock, the commands that
/// change files or records stop without writing; a dry run still runs.
#[cfg(unix)]
#[test]
fn workspace_writers_wait_for_the_workspace_lock() {
    use std::os::unix::io::AsRawFd;
    let (api, ws) = pulled();
    ws.write(SETUP, "#!/bin/sh\necho mine\n");
    let lock = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(ws.path(".nexus/pull.lock"))
        .unwrap();
    // SAFETY: the descriptor belongs to `lock`, which outlives the calls.
    assert_eq!(
        unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) },
        0
    );
    let before = snapshot(&ws);
    for args in [
        &["reset", SETUP][..],
        &["push"][..],
        &["stash", "save"][..],
        &["pull", "--trust"][..],
    ] {
        let run = ws.nexus(&api, args);
        assert!(!run.ok, "{args:?} ran under the lock:\n{}", run.all());
        assert!(
            run.all().contains("is changing this workspace"),
            "{args:?}:\n{}",
            run.all()
        );
    }
    assert_eq!(snapshot(&ws), before, "a locked command wrote");
    // Exports may be read before the lock; nothing is pushed.
    assert!(api.pushes().is_empty(), "a locked push was sent");
    ws.nexus(&api, &["push", "--dry-run"]).assert_ok();

    drop(lock);
    ws.nexus(&api, &["reset", SETUP, "-y"]).assert_ok();
    assert_eq!(ws.read(SETUP), "#!/bin/sh\necho setup\n");
}
