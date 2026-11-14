//! Guard against a repository routing the operator's token elsewhere.
//!
//! When the effective `api_url` comes from a `.nexus/config.toml` that is
//! tracked by git and its origin is not approved (see
//! [`nexus_core::config::trust`]), every command that talks to the API asks
//! first (interactive) or refuses (`--yes`, no terminal), before any request
//! carries a token.

use std::io::IsTerminal;

use console::style;
use nexus_core::config::trust::{self, UnapprovedApiUrl, TRUSTED_API_URLS_ENV};
use nexus_core::config::{Config, ConfigSource, DEFAULT_API_URL};

use crate::{Cli, Command};

/// Commands that never send a token to the API.
fn sends_token(command: &Command) -> bool {
    !matches!(
        command,
        Command::Unlink
            | Command::Logout { .. }
            | Command::Config { .. }
            | Command::Upgrade
            | Command::Man { .. }
    )
}

/// The unapproved repository-pinned API URL in effect for `cli`, if any.
pub(crate) fn unapproved(cli: &Cli) -> Option<UnapprovedApiUrl> {
    let env_url = std::env::var("NEXUS_API_URL")
        .ok()
        .filter(|v| !v.is_empty());
    if cli.api_url.is_some() || env_url.is_some() {
        return None;
    }
    let effective = Config::load_effective_with_provenance(None).ok()?;
    if effective.api_url_source != ConfigSource::Local {
        return None;
    }
    let global = Config::load().unwrap_or_default().api_url;
    let approved = trust::approved_origins(
        [global.as_str(), DEFAULT_API_URL],
        std::env::var(TRUSTED_API_URLS_ENV).ok().as_deref(),
    );
    trust::unapproved_api_url(
        &effective.config.api_url,
        effective.local_path.as_deref(),
        &approved,
        trust::is_git_tracked,
    )
}

/// How to approve the URL, or to stop using it.
fn remedy(u: &UnapprovedApiUrl) -> String {
    format!(
        "Approve it with {TRUSTED_API_URLS_ENV}={origin} (or NEXUS_API_URL / --api-url), \
         set it globally with nexus config set --global api_url={origin}, \
         or remove api_url from {file} if you do not know this host.",
        origin = u.origin,
        file = u.file.display()
    )
}

/// Ask the operator, or refuse outright.
#[derive(Debug, PartialEq, Eq)]
enum Approval {
    Ask,
    Refuse,
}

/// Refuse with `--yes` or without a terminal to ask on.
fn approval(yes: bool, attended: bool) -> Approval {
    if yes || !attended {
        Approval::Refuse
    } else {
        Approval::Ask
    }
}

/// Ask before the first request, or refuse without a terminal or with
/// `--yes`. An approval holds for this process and the commands it starts.
pub(crate) fn guard(cli: &Cli) -> anyhow::Result<()> {
    if !sends_token(&cli.command) {
        return Ok(());
    }
    let Some(u) = unapproved(cli) else {
        return Ok(());
    };
    eprintln!(
        "{} The API URL {} is set by {}, which is tracked by git: the repository chose this host, \
         and your Nexus token would be sent to it.",
        style("!").bold().yellow(),
        style(&u.url).bold(),
        u.file.display()
    );
    let attended = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    if approval(cli.yes, attended) == Approval::Refuse {
        anyhow::bail!(
            "refusing to send credentials to {}, which an untrusted repository file sets. {}",
            u.origin,
            remedy(&u)
        );
    }
    let approved = dialoguer::Confirm::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt(format!("Send your Nexus token to {}?", u.origin))
        .default(false)
        .interact()
        .unwrap_or(false);
    if !approved {
        anyhow::bail!("cancelled; nothing was sent. {}", remedy(&u));
    }
    // Commands started from here (panes, hooks) inherit the approval.
    let list = match std::env::var(TRUSTED_API_URLS_ENV) {
        Ok(v) if !v.trim().is_empty() => format!("{v},{}", u.origin),
        _ => u.origin.clone(),
    };
    std::env::set_var(TRUSTED_API_URLS_ENV, list);
    Ok(())
}

/// The `API URL` source for `nexus status`: `local, tracked by git` when a
/// tracked project config supplies it.
pub(crate) fn source_label(
    source: &str,
    effective: &nexus_core::config::EffectiveConfig,
) -> String {
    let tracked = source == "local"
        && effective
            .local_path
            .as_deref()
            .is_some_and(trust::is_git_tracked);
    if tracked {
        format!("{source}, tracked by git")
    } else {
        source.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn test_only_offline_commands_skip_the_guard() {
        for args in [
            vec!["nexus", "unlink"],
            vec!["nexus", "logout"],
            vec!["nexus", "config", "show"],
            vec!["nexus", "upgrade"],
        ] {
            let cli = Cli::try_parse_from(&args).unwrap();
            assert!(!sends_token(&cli.command), "{args:?}");
        }
        for args in [
            vec!["nexus", "login"],
            vec!["nexus", "status"],
            vec!["nexus", "pull"],
            vec!["nexus", "link"],
            vec!["nexus", "mcp-local"],
        ] {
            let cli = Cli::try_parse_from(&args).unwrap();
            assert!(sends_token(&cli.command), "{args:?}");
        }
    }

    #[test]
    fn test_refused_without_a_terminal_or_with_yes() {
        assert_eq!(approval(false, true), Approval::Ask);
        assert_eq!(approval(true, true), Approval::Refuse);
        assert_eq!(approval(false, false), Approval::Refuse);
    }

    #[test]
    fn test_remedy_names_host_and_file() {
        let u = UnapprovedApiUrl {
            url: "https://attacker.example/".into(),
            origin: "https://attacker.example".into(),
            file: ".nexus/config.toml".into(),
        };
        let r = remedy(&u);
        assert!(r.contains("NEXUS_TRUSTED_API_URLS=https://attacker.example"));
        assert!(r.contains(".nexus/config.toml"));
    }
}
