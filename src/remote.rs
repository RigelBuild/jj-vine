//! Forge detection from a clone's git remotes.
//!
//! Parsing a remote URL into a [`DetectedForge`] (forge type, API host, and
//! `owner/repo` project) is shared between the `init` wizard — which triages
//! origin/upstream/fork to seed prompt defaults — and the runtime config path,
//! which derives `github.project`/`github.host` for the single configured
//! remote when the repo layer left them unset. The two callers differ only in
//! remote selection; the URL parsing is identical, so it lives here.

use itertools::Itertools as _;
use tracing::debug;

use crate::{config::ForgeType, jj::Jujutsu};

/// A forge (type, API host, and project) detected by parsing a remote URL.
#[derive(Debug, Clone)]
pub(crate) struct DetectedForge {
    pub(crate) forge_type: ForgeType,
    pub(crate) host: String,
    pub(crate) project: String,

    /// Only for Azure DevOps, the name of the repository.
    pub(crate) repository_name: Option<String>,
}

/// Parse a forge remote URL to detect forge type, host, and project.
pub(crate) fn parse_forge_url(url: &str) -> Option<DetectedForge> {
    // SSH format: ssh://git@host:owner/repo.git
    // or ssh://git@host/owner/repo.git
    if url.starts_with("git@") || url.starts_with("ssh://git@") {
        let rest = url.trim_start_matches("ssh://").strip_prefix("git@")?;

        let (host, rest) = if let Some((host, rest)) = rest.split_once(':') {
            (host, rest)
        } else {
            let (host, rest) = rest.split_once('/')?;
            (host, rest)
        };

        let forge_type = ForgeType::detect_from_host(host)?;

        let (project, repository_name) = match forge_type {
            ForgeType::AzureDevOps => {
                if let Some((_, org, project, repo)) =
                    rest.trim_end_matches(".git").split('/').collect_tuple()
                {
                    (format!("{org}/{project}"), Some(repo.to_owned()))
                } else {
                    (rest.trim_end_matches(".git").to_owned(), None)
                }
            }
            _ => (rest.trim_end_matches(".git").to_owned(), None),
        };

        let api_host = match forge_type {
            ForgeType::GitHub if host == "github.com" => "https://api.github.com".to_owned(),
            ForgeType::GitHub => format!("https://{host}/api/v3"),
            ForgeType::GitLab | ForgeType::Forgejo | ForgeType::AzureDevOps => {
                format!("https://{host}")
            }
        };

        return Some(DetectedForge {
            forge_type,
            host: api_host,
            project: project.clone(),
            repository_name,
        });
    }

    // HTTPS format: https://host/owner/repo.git
    if url.starts_with("https://") || url.starts_with("http://") {
        let without_protocol = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"))?;

        let (host, path) = without_protocol.split_once('/')?;

        let protocol = if url.starts_with("https://") {
            "https"
        } else {
            "http"
        };

        let forge_type = ForgeType::detect_from_host(host)?;

        let (project, repository_name) = match forge_type {
            ForgeType::AzureDevOps => {
                if let Some((_, org, project, repo)) =
                    path.trim_end_matches(".git").split('/').collect_tuple()
                {
                    (format!("{org}/{project}"), Some(repo.to_owned()))
                } else {
                    (path.trim_end_matches(".git").to_owned(), None)
                }
            }
            _ => (path.trim_end_matches(".git").to_owned(), None),
        };

        let api_host = match forge_type {
            ForgeType::GitHub if host == "github.com" => "https://api.github.com".to_owned(),
            ForgeType::GitHub => format!("{protocol}://{host}/api/v3"),
            ForgeType::GitLab | ForgeType::Forgejo | ForgeType::AzureDevOps => {
                format!("{protocol}://{host}")
            }
        };

        return Some(DetectedForge {
            forge_type,
            host: api_host,
            project: project.clone(),
            repository_name,
        });
    }

    None
}

/// URL-derived forge (project + API host) for the named remote, or `None`
/// when the remote is missing, unparseable, not the requested forge type, or
/// a distinct `upstream` remote is present alongside `remote_name` (the
/// fork-workflow fence — a fork clone's origin is the wrong PR target; see the
/// design record). The returned `DetectedForge` carries both `project` and
/// `host`; the caller fills each config field the repo layer left unset.
/// Subprocess errors are logged at debug and collapse to `None` — derivation
/// is best-effort; the caller's validation produces the actionable error.
pub(crate) fn detect_project(
    jj: &Jujutsu,
    remote_name: &str,
    forge_type: ForgeType,
) -> Option<DetectedForge> {
    let output = match jj.exec(["git", "remote", "list"]) {
        Ok(output) => output,
        Err(e) => {
            debug!("remote derivation: `jj git remote list` failed: {e}");
            return None;
        }
    };

    let mut remote_url = None;
    let mut has_upstream = false;
    for line in output.stdout.lines() {
        // Best-effort by design: skip any line that is not the jj-controlled
        // `name<ws>url` shape, unlike the sibling `detect_remotes`, which
        // hard-errors on a malformed line. This path must fall through to
        // `None` (then static config), never abort a config load.
        let Some((name, url)) = line.split_whitespace().collect_tuple() else {
            continue;
        };
        if name == remote_name {
            remote_url = Some(url);
        }
        if name == "upstream" {
            has_upstream = true;
        }
    }

    // Fork-workflow fence: a distinct `upstream` remote present alongside the
    // configured remote means this is the two-remote fork shape the wizard
    // triages on — the configured remote (default `origin`) is the personal
    // fork, the wrong PR target. Fall through to explicit config.
    if has_upstream && remote_name != "upstream" {
        return None;
    }

    let detected = parse_forge_url(remote_url?)?;

    // Cross-forge fence: a GitHub-forge config whose origin points at a
    // non-GitHub host is detection failure, never a cross-forge fill.
    if detected.forge_type != forge_type {
        return None;
    }

    Some(detected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_gitlab_url_ssh() {
        let url = "git@gitlab.example.com:group/project.git";
        let result = parse_forge_url(url);
        assert!(result.is_some());
        let detected = result.unwrap();
        assert_eq!(detected.forge_type, ForgeType::GitLab);
        assert_eq!(detected.host, "https://gitlab.example.com");
        assert_eq!(detected.project, "group/project");
    }

    #[test]
    fn parse_gitlab_url_https() {
        let url = "https://gitlab.example.com/group/project.git";
        let result = parse_forge_url(url);
        assert!(result.is_some());
        let detected = result.unwrap();
        assert_eq!(detected.forge_type, ForgeType::GitLab);
        assert_eq!(detected.host, "https://gitlab.example.com");
        assert_eq!(detected.project, "group/project");
    }

    #[test]
    fn parse_gitlab_url_nested_groups() {
        let url = "git@gitlab.example.com:group/subgroup/project.git";
        let result = parse_forge_url(url);
        assert!(result.is_some());
        let detected = result.unwrap();
        assert_eq!(detected.forge_type, ForgeType::GitLab);
        assert_eq!(detected.project, "group/subgroup/project");
    }

    #[test]
    fn parse_github_url_ssh() {
        let url = "git@github.com:owner/repo.git";
        let result = parse_forge_url(url);
        assert!(result.is_some());
        let detected = result.unwrap();
        assert_eq!(detected.forge_type, ForgeType::GitHub);
        assert_eq!(detected.host, "https://api.github.com");
        assert_eq!(detected.project, "owner/repo");
    }

    #[test]
    fn parse_github_url_https() {
        let url = "https://github.com/owner/repo.git";
        let result = parse_forge_url(url);
        assert!(result.is_some());
        let detected = result.unwrap();
        assert_eq!(detected.forge_type, ForgeType::GitHub);
        assert_eq!(detected.host, "https://api.github.com");
        assert_eq!(detected.project, "owner/repo");
    }

    #[test]
    fn parse_github_enterprise_ssh() {
        let url = "git@github.example.com:owner/repo.git";
        let result = parse_forge_url(url);
        assert!(result.is_some());
        let detected = result.unwrap();
        assert_eq!(detected.forge_type, ForgeType::GitHub);
        assert_eq!(detected.host, "https://github.example.com/api/v3");
        assert_eq!(detected.project, "owner/repo");
    }

    #[test]
    fn parse_github_enterprise_https() {
        let url = "https://github.example.com/owner/repo.git";
        let result = parse_forge_url(url);
        assert!(result.is_some());
        let detected = result.unwrap();
        assert_eq!(detected.forge_type, ForgeType::GitHub);
        assert_eq!(detected.host, "https://github.example.com/api/v3");
        assert_eq!(detected.project, "owner/repo");
    }

    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    /// A colocated jj test repo. Mirrors `config.rs`'s `create_test_repo` but
    /// without config isolation — `detect_project` reads only `jj git remote
    /// list`, which is repo-local, so no user-config layer can leak into it.
    fn create_test_repo() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let repo_path = temp_dir.path().join("repo");
        std::fs::create_dir_all(&repo_path).expect("Failed to create repo dir");
        let jj = Jujutsu::new(&repo_path).expect("Failed to create Jujutsu instance");
        jj.exec(["git", "init", "--colocate"])
            .expect("Failed to init jj repo");
        (temp_dir, repo_path)
    }

    fn add_remote(repo_path: &Path, name: &str, url: &str) {
        Jujutsu::new(repo_path)
            .expect("Failed to create Jujutsu instance")
            .exec(["git", "remote", "add", name, url])
            .expect("Failed to add remote");
    }

    #[test]
    fn detect_project_named_remote_github_ssh() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "origin", "git@github.com:owner/repo.git");
        let jj = Jujutsu::new(&repo_path).expect("jj");
        let detected =
            detect_project(&jj, "origin", ForgeType::GitHub).expect("detects GitHub origin");
        assert_eq!(detected.project, "owner/repo");
        assert_eq!(detected.host, "https://api.github.com");
    }

    #[test]
    fn detect_project_github_https() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "origin", "https://github.com/owner/repo.git");
        let jj = Jujutsu::new(&repo_path).expect("jj");
        let detected = detect_project(&jj, "origin", ForgeType::GitHub).expect("detects HTTPS");
        assert_eq!(detected.project, "owner/repo");
        assert_eq!(detected.host, "https://api.github.com");
    }

    #[test]
    fn detect_project_strips_trailing_git() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "origin", "git@github.com:owner/repo.git");
        let jj = Jujutsu::new(&repo_path).expect("jj");
        let detected = detect_project(&jj, "origin", ForgeType::GitHub).expect("detects");
        assert_eq!(
            detected.project, "owner/repo",
            "trailing .git must be stripped"
        );
    }

    #[test]
    fn detect_project_missing_remote_is_none() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "origin", "git@github.com:owner/repo.git");
        let jj = Jujutsu::new(&repo_path).expect("jj");
        assert!(detect_project(&jj, "nonexistent", ForgeType::GitHub).is_none());
    }

    #[test]
    fn detect_project_non_forge_host_is_none() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "origin", "git@git.example.com:owner/repo.git");
        let jj = Jujutsu::new(&repo_path).expect("jj");
        assert!(detect_project(&jj, "origin", ForgeType::GitHub).is_none());
    }

    #[test]
    fn detect_project_gitlab_url_requested_as_github_is_none() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "origin", "git@gitlab.com:group/project.git");
        let jj = Jujutsu::new(&repo_path).expect("jj");
        assert!(
            detect_project(&jj, "origin", ForgeType::GitHub).is_none(),
            "cross-forge fence: a GitLab URL must not fill a GitHub config"
        );
    }

    #[test]
    fn detect_project_respects_remote_name_other_than_origin() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "upstream", "git@github.com:canon/repo.git");
        let jj = Jujutsu::new(&repo_path).expect("jj");
        let detected =
            detect_project(&jj, "upstream", ForgeType::GitHub).expect("derives from upstream");
        assert_eq!(detected.project, "canon/repo");
    }

    #[test]
    fn detect_project_fork_shape_origin_and_upstream_is_none() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "origin", "git@github.com:me/fork.git");
        add_remote(&repo_path, "upstream", "git@github.com:canon/repo.git");
        let jj = Jujutsu::new(&repo_path).expect("jj");
        assert!(
            detect_project(&jj, "origin", ForgeType::GitHub).is_none(),
            "fork-workflow fence: a distinct upstream alongside origin skips derivation"
        );
    }

    #[test]
    fn detect_project_explicit_port_ssh_known_limit() {
        // Documented inherited behavior, NOT fixed here: an SSH URL with an
        // explicit port parses through the `split_once(':')` arm to a
        // wrong-but-`Some` project. Not a fleet URL form; pinned so a future
        // change to it is a deliberate, visible decision.
        let (_temp, repo_path) = create_test_repo();
        add_remote(
            &repo_path,
            "origin",
            "ssh://git@github.com:22/owner/repo.git",
        );
        let jj = Jujutsu::new(&repo_path).expect("jj");
        let detected = detect_project(&jj, "origin", ForgeType::GitHub).expect("wrong-but-some");
        assert_eq!(detected.project, "22/owner/repo");
    }

    #[test]
    fn detect_project_subprocess_error_is_none() {
        // The best-effort contract: when `jj git remote list` itself fails to
        // run (here, a cwd that does not exist, so the spawn errors), detection
        // must swallow the error and return `None` — never propagate and abort
        // a config load. Distinct from `detect_project_missing_remote_is_none`,
        // which exercises a *successful* run with no matching remote.
        let temp = TempDir::new().expect("Failed to create temp dir");
        let missing = temp.path().join("does-not-exist");
        let jj = Jujutsu::new(&missing).expect("jj (which() only checks the binary, not cwd)");
        assert!(
            detect_project(&jj, "origin", ForgeType::GitHub).is_none(),
            "a failed `jj git remote list` subprocess falls through to None"
        );
    }
}
