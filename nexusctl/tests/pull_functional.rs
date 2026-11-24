//! Functional tests for `nexus pull`: the real binary against a fake
//! backend in a temporary git workspace. They pin the user-visible
//! behavior: what is written, what is kept, what is removed, and that a
//! dry run or a refused step never changes the workspace.

mod common;
use common::*;

const SETUP: &str = "scripts/devbox/setup.sh";
const CHECK: &str = "scripts/devbox/check.sh";
const SKILL: &str = ".nexus/skills/s0/SKILL.md";

/// A first pull writes every part; a second pull changes nothing and
/// sends nothing but reads.
#[test]
fn pull_writes_everything_and_is_idempotent() {
    let backend = full_backend();
    let (api, ws) = setup(&backend);
    ws.nexus(&api, &["pull", "--trust"])
        .assert_ok()
        .assert_contains("Pull complete");
    for rel in [
        ".nexus/AGENTS.md",
        ".nexus/directives.md",
        ".nexus/TASKS.md",
        SKILL,
        "devbox.json",
        SETUP,
        CHECK,
        "opencode.json",
    ] {
        assert!(ws.exists(rel), "{rel} was not written");
    }
    assert!(ws.read(SKILL).contains("Review things."));
    assert!(ws.read(".nexus/TASKS.md").contains("Fix the bug"));
    assert!(ws.read(".nexus/directives.md").contains("Be precise"));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(ws.path(SETUP))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o100, 0o100, "script is not executable");
    }

    let before = snapshot(&ws);
    api.clear_requests();
    ws.nexus(&api, &["pull", "--trust"]).assert_ok();
    let after = snapshot(&ws);
    let changed: Vec<_> = after
        .iter()
        .filter(|(k, v)| before.get(*k) != Some(*v))
        .map(|(k, _)| k.clone())
        .collect();
    assert!(changed.is_empty(), "second pull changed {changed:?}");
    assert!(api.writes().iter().all(|r| r.body.contains("\"action\"")));
}

/// `--dry-run` reads the backend but leaves the workspace byte for byte.
#[test]
fn dry_run_changes_nothing() {
    let (api, ws) = setup(&full_backend());
    let before = snapshot(&ws);
    ws.nexus(&api, &["pull", "--dry-run"]).assert_ok();
    assert_eq!(snapshot(&ws), before, "fresh workspace changed");

    ws.nexus(&api, &["pull", "--trust"]).assert_ok();
    ws.write(SETUP, "#!/bin/sh\necho mine\n");
    let mut next = full_backend();
    next.scripts.pop();
    next.skills.clear();
    next.install(&api);
    let before = snapshot(&ws);
    ws.nexus(&api, &["pull", "--dry-run"]).assert_ok();
    assert_eq!(snapshot(&ws), before, "pulled workspace changed");
}

/// A local edit survives a pull without --force; --force replaces it.
#[test]
fn local_edits_are_kept_until_forced() {
    let (api, ws) = setup(&full_backend());
    ws.nexus(&api, &["pull", "--trust"]).assert_ok();
    ws.write(SETUP, "#!/bin/sh\necho mine\n");
    ws.write(SKILL, "my skill notes\n");

    let mut next = full_backend();
    next.scripts[0].1 = "#!/bin/sh\necho v2\n".into();
    next.skills[0].1 = "Review things, v2.\n".into();
    next.install(&api);

    ws.nexus(&api, &["pull", "--trust"]).assert_ok();
    assert_eq!(ws.read(SETUP), "#!/bin/sh\necho mine\n");
    assert_eq!(ws.read(SKILL), "my skill notes\n");
    // The untouched script still moves on.
    assert!(ws.read(CHECK).contains("echo check"));

    ws.nexus(&api, &["pull", "--trust", "--force"]).assert_ok();
    assert!(ws.read(SETUP).contains("echo v2"));
    assert!(ws.read(SKILL).contains("v2"));
}

/// A file the backend drops is removed when unchanged and kept when
/// edited; a kept file is not removed by a later pull either.
#[test]
fn dropped_files_go_only_when_unchanged() {
    let (api, ws) = setup(&full_backend());
    ws.nexus(&api, &["pull", "--trust"]).assert_ok();
    ws.write(SETUP, "#!/bin/sh\necho mine\n");

    let mut next = full_backend();
    next.scripts.clear();
    next.skills.clear();
    next.install(&api);
    ws.nexus(&api, &["pull", "--trust"]).assert_ok();

    assert!(!ws.exists(CHECK), "unchanged dropped script stayed");
    assert!(!ws.exists(SKILL), "unchanged dropped skill stayed");
    assert_eq!(ws.read(SETUP), "#!/bin/sh\necho mine\n");

    ws.nexus(&api, &["pull", "--trust"]).assert_ok();
    assert_eq!(ws.read(SETUP), "#!/bin/sh\necho mine\n");
}

/// Executable content needs consent: without a terminal and without
/// --trust it is not written, and nothing else breaks.
#[test]
fn executables_need_consent() {
    let (api, ws) = setup(&full_backend());
    ws.nexus(&api, &["pull"]).assert_ok();
    assert!(!ws.exists(SETUP), "script written without consent");
    assert!(ws.exists(".nexus/AGENTS.md"));
    ws.nexus(&api, &["pull", "--trust"]).assert_ok();
    assert!(ws.exists(SETUP));
}

/// `--scope tasks` writes the task snapshot and nothing else.
#[test]
fn scope_limits_what_is_written() {
    let (api, ws) = setup(&full_backend());
    ws.nexus(&api, &["pull", "--trust", "--scope", "tasks"])
        .assert_ok();
    assert!(ws.exists(".nexus/TASKS.md"));
    for rel in [
        ".nexus/AGENTS.md",
        SKILL,
        "devbox.json",
        SETUP,
        ".nexus/directives.md",
    ] {
        assert!(!ws.exists(rel), "{rel} written outside the scope");
    }
}

/// The task snapshot follows the backend: rewritten when the last task
/// closes; the operator's own TASKS.md is never touched.
#[test]
fn task_snapshot_lifecycle() {
    let (api, ws) = setup(&full_backend());
    ws.nexus(&api, &["pull", "--trust"]).assert_ok();
    let mut next = full_backend();
    next.tasks.clear();
    next.install(&api);
    ws.nexus(&api, &["pull", "--trust"]).assert_ok();
    assert!(!ws.read(".nexus/TASKS.md").contains("Fix the bug"));

    ws.write(".nexus/TASKS.md", "my own list\n");
    ws.nexus(&api, &["pull", "--trust"]).assert_ok();
    assert_eq!(ws.read(".nexus/TASKS.md"), "my own list\n");
}

/// A directives file without the Nexus marker is the operator's.
#[test]
fn user_managed_directives_are_kept() {
    let (api, ws) = setup(&full_backend());
    ws.write(".nexus/directives.md", "# My rules\n");
    ws.nexus(&api, &["pull", "--trust"]).assert_ok();
    assert_eq!(ws.read(".nexus/directives.md"), "# My rules\n");
}

/// `opencode.json` keeps the operator's own settings on a normal pull.
#[test]
fn opencode_json_keeps_operator_settings() {
    let (api, ws) = setup(&full_backend());
    ws.nexus(&api, &["pull", "--trust"]).assert_ok();
    let mut doc: serde_json::Value = serde_json::from_str(&ws.read("opencode.json")).unwrap();
    doc["theme"] = "light".into();
    ws.write(
        "opencode.json",
        &serde_json::to_string_pretty(&doc).unwrap(),
    );
    ws.nexus(&api, &["pull", "--trust"]).assert_ok();
    let doc: serde_json::Value = serde_json::from_str(&ws.read("opencode.json")).unwrap();
    assert_eq!(doc["theme"], "light");
    assert!(doc["mcp"]["nexus"].is_object());
}

/// A pull manifest that cannot be parsed stops the generated-file writes
/// and is never reset.
#[test]
fn broken_pull_manifest_is_not_reset() {
    let (api, ws) = setup(&full_backend());
    ws.nexus(&api, &["pull", "--trust"]).assert_ok();
    let manifest = ".nexus/generated/pull-manifest.json";
    assert!(ws.exists(manifest));
    ws.write(manifest, "{ broken");
    let mut next = full_backend();
    next.skills[0].1 = "Review things, v2.\n".into();
    next.install(&api);
    let _ = ws.nexus(&api, &["pull", "--trust"]);
    assert_eq!(ws.read(manifest), "{ broken");
    assert!(
        !ws.read(SKILL).contains("v2"),
        "skill written past a broken manifest"
    );
}

/// An unreadable workspace file is kept as it is, never replaced as if it
/// were missing.
#[cfg(unix)]
#[test]
fn unreadable_files_are_kept() {
    use std::os::unix::fs::PermissionsExt;
    if unsafe { libc_geteuid() } == 0 {
        return; // root reads everything
    }
    let (api, ws) = setup(&full_backend());
    ws.nexus(&api, &["pull", "--trust"]).assert_ok();
    ws.write(SETUP, "#!/bin/sh\necho mine\n");
    std::fs::set_permissions(ws.path(SETUP), std::fs::Permissions::from_mode(0o000)).unwrap();
    let mut next = full_backend();
    next.scripts[0].1 = "#!/bin/sh\necho v2\n".into();
    next.install(&api);
    ws.nexus(&api, &["pull", "--trust", "--force"])
        .assert_ok()
        .assert_contains("cannot be read");
    std::fs::set_permissions(ws.path(SETUP), std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(ws.read(SETUP), "#!/bin/sh\necho mine\n");
}

#[cfg(unix)]
extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}
