//! Functional tests for `nexus shadow`: what lands in `.git/info/exclude`,
//! and that the git index changes only on an explicit `--untrack`.

mod common;
use common::*;

const EXCLUDE: &str = ".git/info/exclude";

/// A workspace with a committed agent file and a committed source file,
/// and an operator rule of its own in the exclude file.
fn committed() -> (MockApi, Workspace) {
    let (api, ws) = setup(&full_backend());
    ws.write("AGENTS.md", "# root agents\n");
    ws.write("src/main.rs", "fn main() {}\n");
    ws.git(&["add", "AGENTS.md", "src/main.rs"]);
    ws.git(&["commit", "-qm", "init"]);
    let mut exclude = std::fs::read_to_string(ws.path(EXCLUDE)).unwrap_or_default();
    exclude.push_str("my-local-notes/\n");
    ws.write(EXCLUDE, &exclude);
    (api, ws)
}

fn tracked(ws: &Workspace) -> Vec<String> {
    ws.git(&["ls-files"]).lines().map(String::from).collect()
}

/// `on` hides agentic files but never touches the index without
/// `--untrack`; a tracked agent file is only reported.
#[test]
fn shadow_on_does_not_untrack_without_request() {
    let (api, ws) = committed();
    ws.nexus(&api, &["shadow", "on", "--target", "agentic"])
        .assert_ok()
        .assert_contains("--untrack");
    let exclude = ws.read(EXCLUDE);
    assert!(exclude.contains("# >>> nexus shadow"));
    assert!(exclude.contains(".nexus/"));
    assert!(exclude.contains("my-local-notes/"), "operator rule lost");
    assert_eq!(tracked(&ws), vec!["AGENTS.md", "src/main.rs"]);
}

/// `--untrack` takes only the shadowed files out of the index; they stay
/// on disk, other files stay tracked.
#[test]
fn shadow_untrack_takes_only_shadowed_files_out() {
    let (api, ws) = committed();
    ws.nexus(&api, &["shadow", "on", "--target", "agentic", "--untrack"])
        .assert_ok();
    assert_eq!(tracked(&ws), vec!["src/main.rs"]);
    assert_eq!(ws.read("AGENTS.md"), "# root agents\n");
    assert!(ws.git(&["status", "--porcelain"]).contains("D  AGENTS.md"));
}

/// `on` twice writes one block; `off` removes it and keeps the operator's
/// rules.
#[test]
fn shadow_on_is_idempotent_and_off_restores() {
    let (api, ws) = committed();
    let original = ws.read(EXCLUDE);
    ws.nexus(&api, &["shadow", "on", "--target", "agentic"])
        .assert_ok();
    let once = ws.read(EXCLUDE);
    ws.nexus(&api, &["shadow", "on", "--target", "agentic"])
        .assert_ok();
    assert_eq!(ws.read(EXCLUDE), once, "second on changed the exclude file");
    assert_eq!(once.matches("# >>> nexus shadow").count(), 1);

    ws.nexus(&api, &["shadow", "off", "--target", "agentic"])
        .assert_ok();
    let after = ws.read(EXCLUDE);
    assert!(!after.contains("# >>> nexus shadow"));
    assert!(after.contains("my-local-notes/"), "operator rule lost");
    // Lines that were there before stay.
    for line in original.lines() {
        assert!(after.contains(line), "lost {line:?}");
    }
    assert_eq!(tracked(&ws), vec!["AGENTS.md", "src/main.rs"]);
}

/// `--dry-run` changes neither the exclude file nor the index.
#[test]
fn shadow_dry_run_changes_nothing() {
    let (api, ws) = committed();
    let exclude = ws.read(EXCLUDE);
    ws.nexus(
        &api,
        &[
            "shadow",
            "on",
            "--target",
            "agentic",
            "--untrack",
            "--dry-run",
        ],
    )
    .assert_ok();
    assert_eq!(ws.read(EXCLUDE), exclude);
    assert_eq!(tracked(&ws), vec!["AGENTS.md", "src/main.rs"]);
}

/// The workspace target hides the devbox files pull writes, and `status`
/// reports both targets without writing.
#[test]
fn shadow_workspace_target_and_status() {
    let (api, ws) = committed();
    ws.nexus(&api, &["pull", "--trust"]).assert_ok();
    ws.nexus(&api, &["shadow", "on", "--target", "workspace"])
        .assert_ok();
    let untracked = ws.git(&["status", "--porcelain", "--untracked-files=all"]);
    assert!(
        !untracked.contains("devbox.json"),
        "devbox.json visible:\n{untracked}"
    );
    assert!(
        !untracked.contains("scripts/devbox/"),
        "scripts visible:\n{untracked}"
    );
    let before = snapshot(&ws);
    ws.nexus(&api, &["shadow", "status"])
        .assert_ok()
        .assert_contains("Workspace shadow is enabled");
    assert_eq!(snapshot(&ws), before, "status changed the workspace");
}

/// The commit-msg hook with the real binary and a real `git commit` in the
/// editor flow: a comment line is checked when `--cleanup=whitespace` keeps
/// it, and skipped when the default cleanup strips it.
#[cfg(unix)]
#[test]
fn commit_msg_hook_follows_the_cleanup_option() {
    use std::os::unix::fs::PermissionsExt;
    let (_api, ws) = setup(&full_backend());
    let reference = concat!("AD", "R-0042");
    ws.write(
        ".nexus/generated/shadow-guard.json",
        &serde_json::json!({
            "level": "nexus",
            "patterns": [{"id": "nexus-adr", "regex": r"\bADR-\d{3,4}\b", "flags": "i",
                          "scopes": ["commit_message"], "message": "nexus-adr found"}]
        })
        .to_string(),
    );
    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_nexus"));
    ws.write(
        ".git/hooks/commit-msg",
        &format!(
            "#!/bin/sh\nexec '{}' shadow check-msg --guard .nexus/generated/shadow-guard.json \"$1\"\n",
            bin.display()
        ),
    );
    let editor = ws.path(".git/editor.sh");
    std::fs::write(
        &editor,
        format!("#!/bin/sh\nprintf 'fix: x\\n# see {reference}\\n' > \"$1\"\n"),
    )
    .unwrap();
    for p in [ws.path(".git/hooks/commit-msg"), editor.clone()] {
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let commit = |extra: &[&str]| {
        std::process::Command::new("git")
            .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
            .args([
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-q",
                "--allow-empty",
            ])
            .args(extra)
            .current_dir(ws.path(""))
            .env("GIT_EDITOR", &editor)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", ws.path(".git"))
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap()
    };
    let kept = commit(&["--cleanup=whitespace"]);
    assert!(
        !kept.status.success(),
        "a kept comment line was not checked"
    );
    assert!(String::from_utf8_lossy(&kept.stderr).contains("nexus-adr found"));
    let stripped = commit(&[]);
    assert!(
        stripped.status.success(),
        "{}",
        String::from_utf8_lossy(&stripped.stderr)
    );
}
