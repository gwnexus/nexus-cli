//! Operator approval of API URLs a repository pins.
//!
//! A project-local `.nexus/config.toml` that is tracked by git is part of the
//! checked-out repository, so its `api_url` is chosen by whoever wrote the
//! repository. Sending the operator's token there needs the operator's
//! approval: the origin (scheme, host, port) must match the global
//! `~/.config/nexus/config.toml` `api_url`, the compiled-in default, or an
//! entry of `NEXUS_TRUSTED_API_URLS` (comma-separated). `--api-url` and
//! `NEXUS_API_URL` are the operator's own choice and need no approval. These
//! are the rules the Nexus runtime hooks apply, so both sides agree.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Environment variable listing approved API URLs, comma-separated.
pub const TRUSTED_API_URLS_ENV: &str = "NEXUS_TRUSTED_API_URLS";

/// `scheme://host[:port]` of a URL, lower-cased, default ports omitted;
/// `None` when unparsable or without a host.
pub fn url_origin(url: &str) -> Option<String> {
    let parsed = reqwest::Url::parse(url.trim()).ok()?;
    parsed.host_str()?;
    Some(parsed.origin().ascii_serialization().to_lowercase())
}

/// Origins approved for a repository-pinned API URL: the given global and
/// default URLs plus every entry of the trusted list.
pub fn approved_origins<'a>(
    urls: impl IntoIterator<Item = &'a str>,
    trusted: Option<&'a str>,
) -> BTreeSet<String> {
    urls.into_iter()
        .chain(trusted.unwrap_or_default().split(','))
        .filter(|u| !u.trim().is_empty())
        .filter_map(url_origin)
        .collect()
}

/// Whether `path` is tracked by the git repository it lives in. Outside a
/// repository, or when git is unavailable, it is not.
pub fn is_git_tracked(path: &Path) -> bool {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return false;
    };
    std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["ls-files", "--error-unmatch", "--"])
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// A repository-pinned API URL the operator has not approved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnapprovedApiUrl {
    pub url: String,
    pub origin: String,
    /// The tracked `.nexus/config.toml` that sets it.
    pub file: PathBuf,
}

/// The pinned URL when it comes from a tracked project config and its origin
/// is not in `approved`. `local_file` is the project config that supplied the
/// effective `api_url`, `None` when another layer did.
pub fn unapproved_api_url(
    url: &str,
    local_file: Option<&Path>,
    approved: &BTreeSet<String>,
    tracked: impl Fn(&Path) -> bool,
) -> Option<UnapprovedApiUrl> {
    let file = local_file?;
    let origin = url_origin(url).unwrap_or_else(|| url.trim().to_string());
    if approved.contains(&origin) || !tracked(file) {
        return None;
    }
    Some(UnapprovedApiUrl {
        url: url.to_string(),
        origin,
        file: file.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_url_origin() {
        assert_eq!(
            url_origin("https://Nexus.Example/api/"),
            Some("https://nexus.example".into())
        );
        assert_eq!(
            url_origin("https://nexus.example:443"),
            Some("https://nexus.example".into())
        );
        assert_eq!(
            url_origin("http://localhost:3000/x"),
            Some("http://localhost:3000".into())
        );
        assert_eq!(url_origin("not a url"), None);
        assert_eq!(url_origin("data:text/plain,x"), None);
    }

    #[test]
    fn test_approved_origins() {
        let set = approved_origins(
            ["https://global.example/", ""],
            Some(" https://a.example/x , ,bogus,https://b.example:8443"),
        );
        let got: Vec<_> = set.into_iter().collect();
        assert_eq!(
            got,
            vec![
                "https://a.example",
                "https://b.example:8443",
                "https://global.example"
            ]
        );
    }

    #[test]
    fn test_unapproved_api_url() {
        let approved = approved_origins(["https://nexus.example"], Some("https://staging.example"));
        let file = Path::new("/repo/.nexus/config.toml");
        let foreign =
            unapproved_api_url("https://attacker.example/", Some(file), &approved, |_| true)
                .unwrap();
        assert_eq!(foreign.origin, "https://attacker.example");
        assert_eq!(foreign.file, file);
        // Untracked project config, approved origins, or another layer: fine.
        assert!(
            unapproved_api_url("https://attacker.example", Some(file), &approved, |_| false)
                .is_none()
        );
        assert!(
            unapproved_api_url("https://staging.example/", Some(file), &approved, |_| true)
                .is_none()
        );
        assert!(
            unapproved_api_url("https://nexus.example", Some(file), &approved, |_| true).is_none()
        );
        assert!(
            unapproved_api_url("https://attacker.example", None, &approved, |_| true).is_none()
        );
    }

    #[test]
    fn test_is_git_tracked() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .unwrap()
        };
        let file = dir.path().join(".nexus").join("config.toml");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "[config]\n").unwrap();
        assert!(!is_git_tracked(&file), "outside a repository");
        git(&["init", "-q"]);
        assert!(!is_git_tracked(&file), "untracked");
        git(&["add", ".nexus/config.toml"]);
        assert!(is_git_tracked(&file), "tracked");
    }
}
