//! `nexus man`: manual pages generated from the command definitions.
//!
//! The pages carry the same text as `--help`, so they never drift from the
//! binary. Without `--out-dir` the page for `nexus` (or a named subcommand)
//! goes to stdout, ready for `man -l -`; with `--out-dir` one page per
//! visible command is written (`nexus.1`, `nexus-pull.1`,
//! `nexus-dispatch-status.1`, ...), the layout `man` expects under a
//! `man1` directory.

use std::io::Write;
use std::path::Path;

use anyhow::{bail, Context};
use clap::CommandFactory;

use crate::Cli;

/// The fully built `nexus` command, as the pages describe it.
fn root() -> clap::Command {
    let mut cmd = Cli::command()
        .about("Nexus platform operations")
        .disable_help_subcommand(true);
    cmd.build();
    cmd
}

/// The (sub)command named by `path` (`["dispatch", "status"]`), visible ones
/// only.
fn find(root: &clap::Command, path: &[String]) -> anyhow::Result<clap::Command> {
    let mut cmd = root.clone();
    for name in path {
        let next = cmd
            .get_subcommands()
            .filter(|c| !c.is_hide_set())
            .find(|c| c.get_name() == name || c.get_all_aliases().any(|a| a == name))
            .cloned();
        match next {
            Some(c) => cmd = c,
            None => bail!("no command 'nexus {}'", path.join(" ")),
        }
    }
    Ok(cmd)
}

/// Render one page into `out`.
fn render(cmd: clap::Command, out: &mut dyn Write) -> anyhow::Result<()> {
    clap_mangen::Man::new(cmd)
        .render(out)
        .context("could not render the manual page")
}

/// Write every visible command's page into `dir` and return how many.
fn write_all(dir: &Path) -> anyhow::Result<usize> {
    fn walk(cmd: &clap::Command, dir: &Path, count: &mut usize) -> anyhow::Result<()> {
        for sub in cmd.get_subcommands().filter(|c| !c.is_hide_set()) {
            walk(sub, dir, count)?;
        }
        clap_mangen::Man::new(cmd.clone())
            .generate_to(dir)
            .with_context(|| format!("could not write a manual page into {}", dir.display()))?;
        *count += 1;
        Ok(())
    }
    std::fs::create_dir_all(dir).with_context(|| format!("could not create {}", dir.display()))?;
    let mut count = 0;
    walk(&root(), dir, &mut count)?;
    Ok(count)
}

pub fn run(command: &[String], out_dir: Option<&Path>) -> anyhow::Result<()> {
    match out_dir {
        Some(dir) => {
            if !command.is_empty() {
                bail!("--out-dir writes the pages of all commands; leave out the command name");
            }
            let count = write_all(dir)?;
            println!("{count} manual pages written to {}", dir.display());
            println!("Read one with: man {}/nexus.1", dir.display());
        }
        None => {
            let cmd = find(&root(), command)?;
            let mut stdout = std::io::stdout().lock();
            render(cmd, &mut stdout)?;
            stdout.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_writes_one_page_per_visible_command() {
        let dir = tempfile::TempDir::new().unwrap();
        let count = write_all(dir.path()).unwrap();
        assert!(count > 30, "{count}");
        for page in [
            "nexus.1",
            "nexus-pull.1",
            "nexus-dispatch-status.1",
            "nexus-man.1",
        ] {
            assert!(dir.path().join(page).is_file(), "{page}");
        }
        // Hidden commands get no page.
        assert!(!dir.path().join("nexus-mcp-local.1").exists());
        assert!(!dir.path().join("nexus-claude.1").exists());
    }

    #[test]
    fn test_renders_a_subcommand_page_with_its_long_help() {
        let cmd = find(&root(), &["run".to_string()]).unwrap();
        let mut out = Vec::new();
        render(cmd, &mut out).unwrap();
        let page = String::from_utf8(out).unwrap();
        assert!(
            page.contains(".TH nexus-run 1"),
            "{}",
            &page[..200.min(page.len())]
        );
        assert!(page.contains("Start the project"));
        assert!(page.contains("pre\\-launch"));
    }

    #[test]
    fn test_unknown_or_hidden_command_is_an_error() {
        assert!(find(&root(), &["nope".to_string()]).is_err());
        assert!(find(&root(), &["mcp-local".to_string()]).is_err());
        assert!(find(&root(), &["settings".to_string()]).is_ok());
    }
}
