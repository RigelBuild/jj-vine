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
    /// API host, or `None` when the remote names an SSH config alias whose
    /// real host is unknown.
    pub(crate) host: Option<String>,
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

    /// Whether the hostname may be an SSH config `Host` alias rather than a
    /// DNS name, as in `github.com-work` or `github-work`. A DNS name ends in
    /// an alphabetic top-level label; an alias often does not.
    fn is_possible_ssh_alias(&self) -> bool {
        self.transport == Transport::Ssh
            && self.hostname.rsplit_once('.').is_none_or(|(_, tld)| {
                tld.is_empty() || !tld.bytes().all(|byte| byte.is_ascii_alphabetic())
            })
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
///
/// A GitHub Enterprise remote over plain `http://` yields `None`: the API
/// host receives the token, so it is never derived as a plaintext origin.
pub(crate) fn parse_forge_url(url: &str) -> Option<DetectedForge> {
    let remote = RemoteUrl::parse(url)?;
    let is_public_github_host = remote.hostname.eq_ignore_ascii_case("github.com")
        || remote.hostname.eq_ignore_ascii_case("www.github.com");
    let forge_type = if is_public_github_host {
        ForgeType::GitHub
    } else {
        ForgeType::detect_from_host(remote.hostname)?
    };
    let path = project_path(remote.path)?;
    if forge_type == ForgeType::GitHub && path.split('/').count() != 2 {
        return None;
    }

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
        ForgeType::GitHub if is_public_github_host => Some("https://api.github.com".to_owned()),
        ForgeType::GitHub if remote.transport == Transport::Http => return None,
        _ if remote.is_possible_ssh_alias() => None,
        ForgeType::GitHub => Some(format!("{}/api/v3", remote.web_origin())),
        ForgeType::GitLab | ForgeType::Forgejo | ForgeType::AzureDevOps => {
            Some(remote.web_origin())
        }
    };

    Some(DetectedForge {
        forge_type,
        host,
        project,
        repository_name,
    })
}

/// One `jj git remote list` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RemoteListEntry<'a> {
    pub(crate) name: &'a str,
    pub(crate) fetch_url: &'a str,
    /// Set only when the remote has a push URL distinct from its fetch URL.
    pub(crate) push_url: Option<&'a str>,
}

impl<'a> RemoteListEntry<'a> {
    /// The URL that branches are pushed to.
    pub(crate) fn push_or_fetch_url(&self) -> &'a str {
        self.push_url.unwrap_or(self.fetch_url)
    }
}

/// Parse one `jj git remote list` line: `<name> <fetch-url>`, optionally
/// followed by ` (push: <push-url>)`.
pub(crate) fn parse_remote_list_line(line: &str) -> Option<RemoteListEntry<'_>> {
    let mut fields = line.split_whitespace();
    let name = fields.next()?;
    let fetch_url = fields.next()?;
    let push_url = match fields.next() {
        None => None,
        Some("(push:") => Some(fields.next()?.strip_suffix(')')?),
        Some(_) => return None,
    };
    Some(RemoteListEntry {
        name,
        fetch_url,
        push_url,
    })
}

/// Derive forge details for the configured remote from the URL branches are
/// pushed to. Returns `None` for an absent or unrecognized remote, a
/// different forge type, or a fork workflow: a separate `upstream` remote, or
/// `fork` beside `origin` (the layouts `jj-vine init` treats as forks).
/// Detection is best-effort so config validation can report a missing
/// project instead of failing on remote inspection.
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
    let mut has_fork = false;
    let mut has_origin = false;
    for entry in output.stdout.lines().filter_map(parse_remote_list_line) {
        if entry.name == remote_name {
            remote_url = Some(entry.push_or_fetch_url());
        }
        match entry.name {
            "upstream" => has_upstream = true,
            "fork" => has_fork = true,
            "origin" => has_origin = true,
            _ => {}
        }
    }

    // With a separate upstream, origin is normally a fork and not the target.
    if has_upstream && remote_name != "upstream" {
        return None;
    }
    // With `fork` beside `origin`, origin is the canonical target.
    if has_fork && has_origin && remote_name != "origin" {
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
        assert_eq!(detected.host.as_deref(), Some("https://codeberg.org"));
        assert_eq!(detected.project, "owner/repo");
    }

    #[test]
    fn parse_azure_devops_ssh_repository_name() {
        let detected = parse_forge_url("git@ssh.dev.azure.com:v3/organization/project/repo")
            .expect("Azure DevOps URL");
        assert_eq!(detected.forge_type, ForgeType::AzureDevOps);
        assert_eq!(detected.host.as_deref(), Some("https://ssh.dev.azure.com"));
        assert_eq!(detected.project, "organization/project");
        assert_eq!(detected.repository_name.as_deref(), Some("repo"));
    }

    #[test]
    fn parse_github_ssh_url() {
        let detected = parse_forge_url("git@github.com:owner/repo.git").expect("GitHub URL");
        assert_eq!(detected.forge_type, ForgeType::GitHub);
        assert_eq!(detected.host.as_deref(), Some("https://api.github.com"));
        assert_eq!(detected.project, "owner/repo");
    }

    #[test]
    fn parse_github_https_url() {
        let detected = parse_forge_url("https://github.com/owner/repo.git").expect("GitHub URL");
        assert_eq!(detected.forge_type, ForgeType::GitHub);
        assert_eq!(detected.host.as_deref(), Some("https://api.github.com"));
        assert_eq!(detected.project, "owner/repo");
    }

    #[test]
    fn parse_public_github_host_case_and_www_map_to_api() {
        for url in [
            "https://GitHub.com/owner/repo.git",
            "https://WWW.GitHub.com/owner/repo.git",
            "http://GitHub.com/owner/repo.git",
            "http://www.github.com/owner/repo.git",
        ] {
            let detected = parse_forge_url(url).expect("public GitHub URL");
            assert_eq!(detected.forge_type, ForgeType::GitHub, "{url}");
            assert_eq!(
                detected.host.as_deref(),
                Some("https://api.github.com"),
                "{url}"
            );
            assert_eq!(detected.project, "owner/repo", "{url}");
        }
    }

    #[test]
    fn parse_github_enterprise_url() {
        let detected = parse_forge_url("https://github.example.com/owner/repo.git")
            .expect("GitHub Enterprise URL");
        assert_eq!(detected.forge_type, ForgeType::GitHub);
        assert_eq!(
            detected.host.as_deref(),
            Some("https://github.example.com/api/v3")
        );
        assert_eq!(detected.project, "owner/repo");
    }

    #[test]
    fn parse_gitlab_url_ssh() {
        let detected =
            parse_forge_url("git@gitlab.example.com:group/project.git").expect("GitLab SSH URL");
        assert_eq!(detected.forge_type, ForgeType::GitLab);
        assert_eq!(detected.host.as_deref(), Some("https://gitlab.example.com"));
        assert_eq!(detected.project, "group/project");
    }

    #[test]
    fn parse_gitlab_url_https() {
        let detected = parse_forge_url("https://gitlab.example.com/group/project.git")
            .expect("GitLab HTTPS URL");
        assert_eq!(detected.forge_type, ForgeType::GitLab);
        assert_eq!(detected.host.as_deref(), Some("https://gitlab.example.com"));
        assert_eq!(detected.project, "group/project");
    }

    #[test]
    fn parse_github_enterprise_ssh() {
        let detected = parse_forge_url("git@github.example.com:owner/repo.git")
            .expect("GitHub Enterprise SSH URL");
        assert_eq!(detected.forge_type, ForgeType::GitHub);
        assert_eq!(
            detected.host.as_deref(),
            Some("https://github.example.com/api/v3")
        );
        assert_eq!(detected.project, "owner/repo");
    }

    #[test]
    fn parse_gitlab_nested_group_url() {
        let detected =
            parse_forge_url("git@gitlab.example.com:group/subgroup/repo.git").expect("GitLab URL");
        assert_eq!(detected.forge_type, ForgeType::GitLab);
        assert_eq!(detected.host.as_deref(), Some("https://gitlab.example.com"));
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
        assert_eq!(
            detected.host.as_deref(),
            Some("https://github.example.com/api/v3")
        );
    }

    #[test]
    fn parse_https_url_strips_userinfo_from_host() {
        let detected = parse_forge_url("https://user:s3cret@github.com/owner/repo.git")
            .expect("credential-bearing HTTPS URL");
        assert_eq!(detected.forge_type, ForgeType::GitHub);
        assert_eq!(detected.host.as_deref(), Some("https://api.github.com"));
        assert_eq!(detected.project, "owner/repo");
    }

    /// Userinfo never reaches the derived API host; the HTTPS port does.
    #[test]
    fn parse_https_enterprise_userinfo_never_reaches_api_host() {
        let detected = parse_forge_url("https://token@github.example.com:8443/owner/repo.git")
            .expect("credential-bearing Enterprise URL");
        assert_eq!(
            detected.host.as_deref(),
            Some("https://github.example.com:8443/api/v3")
        );
        assert_eq!(detected.project, "owner/repo");
    }

    /// The derived API host receives the token, so a plaintext Enterprise
    /// remote never derives one.
    #[test]
    fn parse_http_enterprise_derives_nothing() {
        assert!(parse_forge_url("http://github.example.com/owner/repo.git").is_none());
        assert!(parse_forge_url("http://github.example.com:8080/owner/repo.git").is_none());
    }

    /// github.com always maps to the HTTPS public API, whatever the transport.
    #[test]
    fn parse_http_github_com_uses_https_api() {
        let detected = parse_forge_url("http://github.com/owner/repo.git").expect("GitHub URL");
        assert_eq!(detected.host.as_deref(), Some("https://api.github.com"));
    }

    /// An SSH config alias names no real host, so it yields the project only.
    #[test]
    fn parse_ssh_alias_derives_project_without_host() {
        for url in [
            "git@github.com-work:owner/repo.git",
            "git@github-personal:owner/repo.git",
        ] {
            let detected = parse_forge_url(url).expect("GitHub alias URL");
            assert_eq!(detected.forge_type, ForgeType::GitHub, "{url}");
            assert_eq!(detected.host, None, "{url}");
            assert_eq!(detected.project, "owner/repo", "{url}");
        }
    }

    #[test]
    fn parse_remote_list_line_reads_push_url() {
        assert_eq!(
            parse_remote_list_line("origin git@github.com:o/r.git (push: git@github.com:p/r.git)"),
            Some(RemoteListEntry {
                name: "origin",
                fetch_url: "git@github.com:o/r.git",
                push_url: Some("git@github.com:p/r.git"),
            })
        );
        let entry = parse_remote_list_line("origin git@github.com:o/r.git").expect("entry");
        assert_eq!(entry.push_url, None);
        assert_eq!(entry.push_or_fetch_url(), "git@github.com:o/r.git");
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

    #[test]
    fn parse_github_rejects_paths_beyond_owner_and_repo() {
        for url in [
            "https://github.com/owner/repo/issues",
            "git@github.com:owner/repo/subdir.git",
            "https://github.example.com/owner/repo/subdir.git",
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

    /// Branches go to the push URL, so it names the pushed project.
    #[test]
    fn detect_remote_with_separate_push_url() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "origin", "git@github.com:owner/repo.git");
        set_push_url(&repo_path, "origin", "git@github.com:person/push.git");
        let jj = Jujutsu::new(&repo_path).expect("jj");
        let detected = detect_project(&jj, "origin", ForgeType::GitHub).expect("detect remote");
        assert_eq!(detected.project, "person/push");
    }

    /// `fork` beside `origin` is the layout init treats as a fork workflow
    /// with origin as the canonical target.
    #[test]
    fn detect_fork_remote_beside_origin_returns_none() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "origin", "git@github.com:owner/repo.git");
        add_remote(&repo_path, "fork", "git@github.com:person/fork.git");
        let jj = Jujutsu::new(&repo_path).expect("jj");
        assert!(detect_project(&jj, "fork", ForgeType::GitHub).is_none());
    }

    #[test]
    fn detect_origin_beside_fork_remote_derives_canonical() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "origin", "git@github.com:owner/repo.git");
        add_remote(&repo_path, "fork", "git@github.com:person/fork.git");
        let jj = Jujutsu::new(&repo_path).expect("jj");
        let detected = detect_project(&jj, "origin", ForgeType::GitHub).expect("detect origin");
        assert_eq!(detected.project, "owner/repo");
        assert_eq!(detected.host.as_deref(), Some("https://api.github.com"));
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
