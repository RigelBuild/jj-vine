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

/// Transport of a parsed remote URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Transport {
    Ssh,
    Http,
    Https,
}

/// The parts of a remote URL that forge detection uses. Userinfo is never
/// kept, so no field can carry a credential embedded in the remote.
#[derive(Debug)]
struct RemoteUrl<'a> {
    transport: Transport,
    hostname: &'a str,
    port: Option<u16>,
    path: &'a str,
}

impl<'a> RemoteUrl<'a> {
    /// Parse `git@host:path`, `ssh://git@host[:port]/path`, or
    /// `http[s]://[userinfo@]host[:port]/path`.
    fn parse(url: &'a str) -> Option<Self> {
        if let Some(rest) = url.strip_prefix("ssh://") {
            let (authority, path) = rest.split_once('/')?;
            let host_port = authority.strip_prefix("git@")?;
            let (hostname, port) = split_host_port(host_port)?;
            return Some(Self {
                transport: Transport::Ssh,
                hostname,
                port,
                path,
            });
        }

        if let Some(rest) = url.strip_prefix("git@") {
            // scp-like syntax has no port; the text after ':' is the path.
            let (hostname, path) = rest.split_once(':')?;
            return Some(Self {
                transport: Transport::Ssh,
                hostname: valid_hostname(hostname)?,
                port: None,
                path,
            });
        }

        let (transport, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (Transport::Https, rest)
        } else {
            (Transport::Http, url.strip_prefix("http://")?)
        };
        // A query or fragment has no meaning for a clone URL.
        if rest.contains(['?', '#']) {
            return None;
        }
        let (authority, path) = rest.split_once('/')?;
        // Userinfo ends at the last '@'; the host is only what follows it.
        let host_port = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host_port)| host_port);
        let (hostname, port) = split_host_port(host_port)?;
        Some(Self {
            transport,
            hostname,
            port,
            path,
        })
    }

    /// Origin (`scheme://host[:port]`) of the forge web service. An SSH port
    /// belongs to the Git transport, so SSH remotes map to default HTTPS.
    fn web_origin(&self) -> String {
        match (self.transport, self.port) {
            (Transport::Ssh, _) => format!("https://{}", self.hostname),
            (Transport::Http, None) => format!("http://{}", self.hostname),
            (Transport::Http, Some(port)) => format!("http://{}:{port}", self.hostname),
            (Transport::Https, None) => format!("https://{}", self.hostname),
            (Transport::Https, Some(port)) => format!("https://{}:{port}", self.hostname),
        }
    }
}

/// Split `host[:port]`. A present port must be a valid, non-empty `u16`.
fn split_host_port(host_port: &str) -> Option<(&str, Option<u16>)> {
    match host_port.split_once(':') {
        Some((hostname, port)) => Some((valid_hostname(hostname)?, Some(port.parse().ok()?))),
        None => Some((valid_hostname(host_port)?, None)),
    }
}

/// Accept a DNS-style hostname only: no userinfo, port, brackets, or path.
fn valid_hostname(hostname: &str) -> Option<&str> {
    let valid = !hostname.is_empty()
        && hostname
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.');
    valid.then_some(hostname)
}

/// Strip `.git` and one trailing `/`, then require at least two non-empty
/// path segments.
fn project_path(path: &str) -> Option<&str> {
    let path = path.strip_suffix('/').unwrap_or(path);
    let path = path.strip_suffix(".git").unwrap_or(path);
    let segments_valid = path.contains('/') && path.split('/').all(|segment| !segment.is_empty());
    segments_valid.then_some(path)
}

/// Parse a forge remote URL to detect forge type, host, and project.
pub(crate) fn parse_forge_url(url: &str) -> Option<DetectedForge> {
    let remote = RemoteUrl::parse(url)?;
    let forge_type = ForgeType::detect_from_host(remote.hostname)?;
    let path = project_path(remote.path)?;

    let (project, repository_name) = match forge_type {
        ForgeType::AzureDevOps => {
            if let Some((_, org, project, repo)) = path.split('/').collect_tuple() {
                (format!("{org}/{project}"), Some(repo.to_owned()))
            } else {
                (path.to_owned(), None)
            }
        }
        ForgeType::GitHub | ForgeType::GitLab | ForgeType::Forgejo => (path.to_owned(), None),
    };

    let host = match forge_type {
        ForgeType::GitHub if remote.hostname == "github.com" => "https://api.github.com".to_owned(),
        // RIG-4484 (open): this derives a token-bearing API host from any
        // remote host that ForgeType::detect_from_host classifies as GitHub,
        // including HTTP. Not a trusted credential endpoint until decided.
        ForgeType::GitHub => format!("{}/api/v3", remote.web_origin()),
        ForgeType::GitLab | ForgeType::Forgejo | ForgeType::AzureDevOps => remote.web_origin(),
    };

    Some(DetectedForge {
        forge_type,
        host,
        project,
        repository_name,
    })
}

/// One `jj git remote list` entry: `<name> <fetch-url>`, optionally followed
/// by ` (push: <push-url>)`. Returns the name and the fetch URL.
fn parse_remote_list_line(line: &str) -> Option<(&str, &str)> {
    let mut fields = line.split_whitespace();
    let name = fields.next()?;
    let fetch_url = fields.next()?;
    Some((name, fetch_url))
}

/// Derive forge details for the configured remote. Returns `None` for an
/// absent or unrecognized remote, a different forge type, or a clone with a
/// separate `upstream` remote. Detection is best-effort so config validation
/// can report a missing project instead of failing on remote inspection.
///
/// Remote URLs can embed credentials, so they are never logged.
pub(crate) fn detect_project(
    jj: &Jujutsu,
    remote_name: &str,
    forge_type: ForgeType,
) -> Option<DetectedForge> {
    let output = match jj.exec_redacted(["git", "remote", "list"]) {
        Ok(output) => output,
        Err(error) => {
            debug!("remote derivation: `jj git remote list` failed: {error}");
            return None;
        }
    };

    let mut remote_url = None;
    let mut has_upstream = false;
    for (name, url) in output.stdout.lines().filter_map(parse_remote_list_line) {
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

    /// Pins remote-derived GitHub Enterprise API hosts, which receive the
    /// configured token. Whether that host may be trusted is undecided.
    #[test]
    #[ignore = "RIG-4484: GHE token-host trust undecided; not a safety claim"]
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
    #[ignore = "RIG-4484: GHE token-host trust undecided; not a safety claim"]
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

    /// Policy-independent: the SSH port never becomes a project segment and
    /// never reaches the API host.
    #[test]
    fn parse_ssh_url_with_port_keeps_project_path() {
        let detected = parse_forge_url("ssh://git@github.example.com:2222/owner/repo.git")
            .expect("port-bearing SSH URL");
        assert_eq!(detected.forge_type, ForgeType::GitHub);
        assert_eq!(detected.project, "owner/repo");
        assert!(
            !detected.host.contains("2222"),
            "SSH port leaked: {}",
            detected.host
        );
    }

    #[test]
    fn parse_https_url_strips_userinfo_from_host() {
        let detected = parse_forge_url("https://user:s3cret@github.com/owner/repo.git")
            .expect("credential-bearing HTTPS URL");
        assert_eq!(detected.forge_type, ForgeType::GitHub);
        assert_eq!(detected.host, "https://api.github.com");
        assert_eq!(detected.project, "owner/repo");
    }

    /// Policy-independent: userinfo never reaches the derived host, whatever
    /// RIG-4484 decides about trusting that host.
    #[test]
    fn parse_https_enterprise_userinfo_never_reaches_api_host() {
        let detected = parse_forge_url("https://token@github.example.com:8443/owner/repo.git")
            .expect("credential-bearing Enterprise URL");
        assert!(
            !detected.host.contains('@'),
            "userinfo leaked: {}",
            detected.host
        );
        assert!(
            !detected.host.contains("token"),
            "userinfo leaked: {}",
            detected.host
        );
        assert_eq!(detected.project, "owner/repo");
    }

    #[test]
    fn parse_https_userinfo_does_not_select_forge() {
        // The real host is attacker.example; "github.com" is only userinfo.
        assert!(parse_forge_url("https://github.com@attacker.example/owner/repo.git").is_none());
    }

    #[test]
    fn parse_rejects_malformed_authority_and_path() {
        for url in [
            "https:///owner/repo.git",
            "https://github.com:notaport/owner/repo.git",
            "https://github.com/",
            "https://github.com/owner//repo.git",
            "https://github.com/owner/repo.git?x=1",
            "ssh://git@github.com:/owner/repo.git",
            "git@github.com:",
        ] {
            assert!(parse_forge_url(url).is_none(), "must reject {url}");
        }
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

    fn set_push_url(repo_path: &Path, name: &str, url: &str) {
        Jujutsu::new(repo_path)
            .expect("create Jujutsu instance")
            .exec(["git", "remote", "set-url", name, "--push", url])
            .expect("set push URL");
    }

    #[test]
    fn detect_remote_with_separate_push_url() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "origin", "git@github.com:owner/repo.git");
        set_push_url(&repo_path, "origin", "git@github.com:owner/push.git");
        let jj = Jujutsu::new(&repo_path).expect("jj");
        let detected = detect_project(&jj, "origin", ForgeType::GitHub).expect("detect remote");
        assert_eq!(detected.project, "owner/repo");
    }

    #[test]
    fn detect_fence_applies_when_upstream_has_push_url() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "origin", "git@github.com:person/fork.git");
        add_remote(&repo_path, "upstream", "git@github.com:owner/repo.git");
        set_push_url(&repo_path, "upstream", "DISABLE");
        let jj = Jujutsu::new(&repo_path).expect("jj");
        assert!(detect_project(&jj, "origin", ForgeType::GitHub).is_none());
    }

    /// Captures every tracing event at TRACE level into a shared buffer.
    #[derive(Clone, Default)]
    struct CapturedLogs(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for CapturedLogs {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("log buffer").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl CapturedLogs {
        fn capture(&self, f: impl FnOnce()) -> String {
            let writer = self.clone();
            let subscriber = tracing_subscriber::fmt()
                .with_max_level(tracing::Level::TRACE)
                .with_writer(move || writer.clone())
                .finish();
            tracing::subscriber::with_default(subscriber, f);
            String::from_utf8(self.0.lock().expect("log buffer").clone()).expect("utf-8 logs")
        }
    }

    #[test]
    fn detect_never_traces_remote_userinfo() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(
            &repo_path,
            "origin",
            "https://user:s3cret-token@github.com/owner/repo.git",
        );
        let jj = Jujutsu::new(&repo_path).expect("jj");

        let logs = CapturedLogs::default().capture(|| {
            let detected = detect_project(&jj, "origin", ForgeType::GitHub).expect("detect");
            assert_eq!(detected.project, "owner/repo");
        });

        // The subscriber must observe the command, or the check proves nothing.
        assert!(
            logs.contains("git remote list"),
            "trace capture is live: {logs}"
        );
        assert!(
            !logs.contains("s3cret-token"),
            "remote userinfo leaked: {logs}"
        );
    }

    #[test]
    fn detect_failure_never_traces_remote_userinfo() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(
            &repo_path,
            "origin",
            "https://user:s3cret-token@github.com/owner/repo.git",
        );
        let jj = Jujutsu::new(&repo_path).expect("jj");

        // A forge mismatch exercises the not-derived path after listing.
        let logs = CapturedLogs::default().capture(|| {
            assert!(detect_project(&jj, "origin", ForgeType::GitLab).is_none());
        });

        assert!(
            logs.contains("git remote list"),
            "trace capture is live: {logs}"
        );
        assert!(
            !logs.contains("s3cret-token"),
            "remote userinfo leaked: {logs}"
        );
    }

    #[test]
    fn detect_remote_inspection_error_returns_none() {
        let temp = TempDir::new().expect("create temp directory");
        let missing = temp.path().join("does-not-exist");
        let jj = Jujutsu::new(&missing).expect("jj binary is available");
        assert!(detect_project(&jj, "origin", ForgeType::GitHub).is_none());
    }
}
