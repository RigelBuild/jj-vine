//! Forge detection from a clone's Git remotes.
//!
//! URL parsing is shared by initialization and runtime config derivation.

use itertools::Itertools as _;
use tracing::debug;

use crate::{config::ForgeType, jj::Jujutsu};

/// Forge details parsed from a Git remote URL.
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
    // SSH format: git@host:owner/repo.git or ssh://git@host/owner/repo.git
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
            project,
            repository_name,
        });
    }

    // HTTP(S) format: https://host/owner/repo.git
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
            project,
            repository_name,
        });
    }

    None
}

/// Derive forge details for the configured remote. Returns `None` for an
/// absent or unrecognized remote, a different forge type, or a clone with a
/// separate `upstream` remote. Detection is best-effort so config validation
/// can report a missing project instead of failing on remote inspection.
pub(crate) fn detect_project(
    jj: &Jujutsu,
    remote_name: &str,
    forge_type: ForgeType,
) -> Option<DetectedForge> {
    let output = match jj.exec(["git", "remote", "list"]) {
        Ok(output) => output,
        Err(error) => {
            debug!("remote derivation: `jj git remote list` failed: {error}");
            return None;
        }
    };

    let mut remote_url = None;
    let mut has_upstream = false;
    for line in output.stdout.lines() {
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

    // With a separate upstream, origin is normally a fork and not the target.
    if has_upstream && remote_name != "upstream" {
        return None;
    }

    let detected = parse_forge_url(remote_url?)?;
    (detected.forge_type == forge_type).then_some(detected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_forgejo_url() {
        let detected = parse_forge_url("https://codeberg.org/owner/repo.git").expect("Forgejo URL");
        assert_eq!(detected.forge_type, ForgeType::Forgejo);
        assert_eq!(detected.host, "https://codeberg.org");
        assert_eq!(detected.project, "owner/repo");
    }

    #[test]
    fn parse_azure_devops_ssh_repository_name() {
        let detected = parse_forge_url("git@ssh.dev.azure.com:v3/organization/project/repo")
            .expect("Azure DevOps URL");
        assert_eq!(detected.forge_type, ForgeType::AzureDevOps);
        assert_eq!(detected.host, "https://ssh.dev.azure.com");
        assert_eq!(detected.project, "organization/project");
        assert_eq!(detected.repository_name.as_deref(), Some("repo"));
    }

    #[test]
    fn parse_github_ssh_url() {
        let detected = parse_forge_url("git@github.com:owner/repo.git").expect("GitHub URL");
        assert_eq!(detected.forge_type, ForgeType::GitHub);
        assert_eq!(detected.host, "https://api.github.com");
        assert_eq!(detected.project, "owner/repo");
    }

    #[test]
    fn parse_github_https_url() {
        let detected = parse_forge_url("https://github.com/owner/repo.git").expect("GitHub URL");
        assert_eq!(detected.forge_type, ForgeType::GitHub);
        assert_eq!(detected.host, "https://api.github.com");
        assert_eq!(detected.project, "owner/repo");
    }

    #[test]
    fn parse_github_enterprise_url() {
        let detected = parse_forge_url("https://github.example.com/owner/repo.git")
            .expect("GitHub Enterprise URL");
        assert_eq!(detected.forge_type, ForgeType::GitHub);
        assert_eq!(detected.host, "https://github.example.com/api/v3");
        assert_eq!(detected.project, "owner/repo");
    }

    #[test]
    fn parse_gitlab_url_ssh() {
        let detected =
            parse_forge_url("git@gitlab.example.com:group/project.git").expect("GitLab SSH URL");
        assert_eq!(detected.forge_type, ForgeType::GitLab);
        assert_eq!(detected.host, "https://gitlab.example.com");
        assert_eq!(detected.project, "group/project");
    }

    #[test]
    fn parse_gitlab_url_https() {
        let detected = parse_forge_url("https://gitlab.example.com/group/project.git")
            .expect("GitLab HTTPS URL");
        assert_eq!(detected.forge_type, ForgeType::GitLab);
        assert_eq!(detected.host, "https://gitlab.example.com");
        assert_eq!(detected.project, "group/project");
    }

    #[test]
    fn parse_github_enterprise_ssh() {
        let detected = parse_forge_url("git@github.example.com:owner/repo.git")
            .expect("GitHub Enterprise SSH URL");
        assert_eq!(detected.forge_type, ForgeType::GitHub);
        assert_eq!(detected.host, "https://github.example.com/api/v3");
        assert_eq!(detected.project, "owner/repo");
    }

    #[test]
    fn parse_gitlab_nested_group_url() {
        let detected =
            parse_forge_url("git@gitlab.example.com:group/subgroup/repo.git").expect("GitLab URL");
        assert_eq!(detected.forge_type, ForgeType::GitLab);
        assert_eq!(detected.host, "https://gitlab.example.com");
        assert_eq!(detected.project, "group/subgroup/repo");
    }

    #[test]
    fn parse_unknown_remote_url() {
        assert!(parse_forge_url("git@git.example.com:owner/repo.git").is_none());
    }

    use std::path::{Path, PathBuf};

    use tempfile::TempDir;

    fn create_test_repo() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        let repo_path = temp_dir.path().join("repo");
        std::fs::create_dir_all(&repo_path).expect("create repo directory");
        let jj = Jujutsu::new(&repo_path).expect("create Jujutsu instance");
        jj.exec(["git", "init", "--colocate"])
            .expect("initialize jj repo");
        (temp_dir, repo_path)
    }

    fn add_remote(repo_path: &Path, name: &str, url: &str) {
        Jujutsu::new(repo_path)
            .expect("create Jujutsu instance")
            .exec(["git", "remote", "add", name, url])
            .expect("add remote");
    }

    #[test]
    fn detect_configured_remote() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "origin", "git@github.com:owner/repo.git");
        let jj = Jujutsu::new(&repo_path).expect("jj");
        let detected = detect_project(&jj, "origin", ForgeType::GitHub).expect("detect remote");
        assert_eq!(detected.project, "owner/repo");
    }

    #[test]
    fn detect_missing_remote_returns_none() {
        let (_temp, repo_path) = create_test_repo();
        let jj = Jujutsu::new(&repo_path).expect("jj");
        assert!(detect_project(&jj, "origin", ForgeType::GitHub).is_none());
    }

    #[test]
    fn detect_different_forge_returns_none() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "origin", "git@gitlab.com:group/repo.git");
        let jj = Jujutsu::new(&repo_path).expect("jj");
        assert!(detect_project(&jj, "origin", ForgeType::GitHub).is_none());
    }

    #[test]
    fn detect_remote_name_other_than_origin() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "upstream", "git@github.com:owner/repo.git");
        let jj = Jujutsu::new(&repo_path).expect("jj");
        let detected = detect_project(&jj, "upstream", ForgeType::GitHub).expect("detect upstream");
        assert_eq!(detected.project, "owner/repo");
    }

    #[test]
    fn detect_origin_fork_with_upstream_returns_none() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "origin", "git@github.com:person/fork.git");
        add_remote(&repo_path, "upstream", "git@github.com:owner/repo.git");
        let jj = Jujutsu::new(&repo_path).expect("jj");
        assert!(detect_project(&jj, "origin", ForgeType::GitHub).is_none());
    }

    #[test]
    fn detect_remote_inspection_error_returns_none() {
        let temp = TempDir::new().expect("create temp directory");
        let missing = temp.path().join("does-not-exist");
        let jj = Jujutsu::new(&missing).expect("jj binary is available");
        assert!(detect_project(&jj, "origin", ForgeType::GitHub).is_none());
    }
}
