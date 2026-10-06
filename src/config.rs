#![expect(clippy::module_name_repetitions, reason = "fine for Config")]

use std::path::PathBuf;

use bon::Builder;
use serde::{Deserialize, de::Visitor};

use crate::{
    error::{ConfigSnafu, Error, Result},
    jj::Jujutsu,
    remote::{ApiHost, FetchTarget, same_api_host},
};

/// Forge type (GitLab, GitHub, or Forgejo).
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, strum::VariantArray)]
#[serde(rename_all = "lowercase")]
pub enum ForgeType {
    /// GitLab (GitLab.com or self-hosted).
    GitLab,

    /// GitHub (GitHub.com or GitHub Enterprise).
    GitHub,

    /// Forgejo/Gitea (self-hosted or Codeberg).
    Forgejo,

    /// Azure DevOps.
    #[serde(rename = "azure")]
    AzureDevOps,
}

impl ForgeType {
    #[must_use]
    pub fn detect_from_host(host: &str) -> Option<Self> {
        if host.contains("gitlab") {
            Some(Self::GitLab)
        } else if host.contains("github") {
            Some(Self::GitHub)
        } else if host.contains("forgejo") || host.contains("gitea") || host.contains("codeberg") {
            Some(Self::Forgejo)
        } else if host.contains("azure") {
            Some(Self::AzureDevOps)
        } else {
            None
        }
    }

    #[must_use]
    pub fn display_name(&self) -> &str {
        match self {
            ForgeType::GitLab => "GitLab",
            ForgeType::GitHub => "GitHub",
            ForgeType::Forgejo => "Forgejo",
            ForgeType::AzureDevOps => "Azure DevOps",
        }
    }
}

impl core::fmt::Display for ForgeType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                ForgeType::GitLab => "gitlab",
                ForgeType::GitHub => "github",
                ForgeType::Forgejo => "forgejo",
                ForgeType::AzureDevOps => "azure",
            }
        )
    }
}

impl core::str::FromStr for ForgeType {
    type Err = Error;

    fn from_str(s: &str) -> core::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "gitlab" => Ok(Self::GitLab),
            "github" => Ok(Self::GitHub),
            "forgejo" => Ok(Self::Forgejo),
            "azure" => Ok(Self::AzureDevOps),
            _ => Err(ConfigSnafu {
                message: format!("Invalid forge type: {s}"),
            }
            .build()),
        }
    }
}

/// Stack visualization format.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum DescriptionDiagramFormat {
    /// Do not render this stack visualization.
    None,

    /// A linear numbered list.
    #[default]
    Linear,

    /// A tree of MRs where children are indented.
    Tree,
}

fn default_remote_name() -> String {
    "origin".to_owned()
}

const fn default_true() -> bool {
    true
}

/// Configuration for jj-vine.
#[derive(Debug, Clone, Deserialize, Builder)]
#[serde(rename_all = "camelCase")]
#[expect(clippy::struct_excessive_bools, reason = "deserialized")]
pub struct Config {
    /// Which forge to use.
    pub forge: ForgeType,

    /// The base branch for root MRs in a stack. When unset, jj-vine uses the
    /// bookmark that `trunk()` resolves to.
    #[serde(default)]
    pub default_base_branch: Option<String>,

    // ===== Common Configuration =====
    /// Git remote name (defaults to "origin").
    #[serde(default = "default_remote_name")]
    #[builder(default = default_remote_name())]
    pub remote_name: String,

    /// Optional path to CA bundle for TLS verification. Only useful if you
    /// have a self-hosted forge without a publicly trusted certificate.
    #[serde(default)]
    pub ca_bundle: Option<String>,

    /// Accept non-compliant TLS certificates (for certificates that don't meet
    /// strict X.509 standards). This is almost always unnecessary unless
    /// you have a unique situation.
    #[serde(default)]
    #[builder(default)]
    pub tls_accept_non_compliant_certs: bool,

    /// Configuration for MR description generation.
    #[serde(default)]
    #[builder(default)]
    pub description: DescriptionConfig,

    /// Delete source branch when MR is merged (defaults to true).
    ///
    /// Unsupported by GitHub and Forgejo - this option will have no effect.
    /// Those forges only offer this as a repository-level default +
    /// on-merge flag.
    #[serde(default = "default_true")]
    #[builder(default)]
    pub delete_source_branch: bool,

    /// Squash commits when MR is merged (defaults to false).
    ///
    /// Unsupported by GitHub and Forgejo - this option will have no effect.
    /// Those forges only offer this as a repository-level default +
    /// on-merge flag.
    #[serde(default)]
    #[builder(default)]
    pub squash_commits: bool,

    /// Assign created MRs to yourself (defaults to false).
    #[serde(default)]
    #[builder(default)]
    pub assign_to_self: bool,

    /// Default reviewers for created MRs (list of usernames).
    #[serde(default)]
    #[builder(default)]
    pub default_reviewers: Vec<String>,

    /// Open newly created MRs as drafts (defaults to false).
    ///
    /// On Forgejo and GitLab, this will add "WIP: " and "Draft: " prefixes to
    /// the MR titles, respectively. This is configurable for Forgejo using the
    /// `jj-vine.forgejo.wip_prefix` setting.
    #[serde(default)]
    #[builder(default)]
    pub open_as_draft: bool,

    /// Whether to fetch the remote before planning a submission, or the jj
    /// command to run prior to planning. Defaults to true, which is
    /// equivalent to `jj git fetch --tracked`. This may also be set to an
    /// array, for example `["git", "fetch"]` will run `jj git fetch`. Set
    /// to false to disable fetching completely.
    ///
    /// If disabled, jj-vine may recreate deleted bookmarks, so be careful when
    /// disabling this.
    #[serde(default)]
    #[builder(default)]
    pub fetch: RepoFetchConfig,

    /// Push command to run. Defaults to `jj git push`; false disables pushing.
    ///
    /// An array sets a complete argv. The remote and bookmark or change
    /// arguments are appended, so the command must accept the corresponding
    /// `jj git push` flags. `submit --no-hooks` runs the built-in command
    /// instead.
    #[serde(default)]
    #[builder(default)]
    pub push: RepoPushConfig,

    /// Configuration for MR title generation.
    #[serde(default)]
    #[builder(default)]
    pub title: TitleConfig,

    /// GitLab configuration.
    #[serde(default)]
    #[builder(default)]
    pub gitlab: GitLabConfig,

    /// GitHub configuration. Empty `host` and `project` values may be derived
    /// from the configured Git remote when loading repository config.
    #[serde(default)]
    #[builder(default)]
    pub github: GitHubConfig,

    /// Forgejo/Gitea/Codeberg configuration.
    #[serde(default)]
    #[builder(default)]
    pub forgejo: ForgejoConfig,

    /// Azure DevOps configuration.
    #[serde(default)]
    #[builder(default)]
    pub azure: AzureDevOpsConfig,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum RepoFetchConfig {
    /// If true, is equivalent to `["git", "fetch", "--tracked"]`. If false,
    /// fetching before planning is disabled.
    Enabled(bool),

    /// Runs `jj` with these arguments before planning a submission.
    Command(Vec<String>),
}

impl RepoFetchConfig {
    pub fn to_args(&self) -> Option<Vec<&str>> {
        match self {
            RepoFetchConfig::Enabled(true) => Some(vec!["git", "fetch", "--tracked"]),
            RepoFetchConfig::Command(command) => Some(command.iter().map(String::as_str).collect()),
            RepoFetchConfig::Enabled(false) => None,
        }
    }
}

impl Default for RepoFetchConfig {
    fn default() -> Self {
        Self::Enabled(true)
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum RepoPushConfig {
    /// If true, run the built-in `jj git push`; false disables pushing.
    Enabled(bool),

    /// Runs this full argv instead of the built-in push command.
    ///
    /// The remote and bookmark or change arguments are appended, so custom
    /// commands must accept the corresponding `jj git push` flags.
    Command(Vec<String>),
}

impl RepoPushConfig {
    /// Return the configured push argv, or `None` when pushing is disabled.
    #[must_use]
    pub fn to_argv(&self) -> Option<Vec<String>> {
        match self {
            Self::Enabled(true) => Some(Self::builtin_push_argv()),
            Self::Enabled(false) => None,
            Self::Command(command) => Some(command.clone()),
        }
    }

    /// Resolve the push argv for this run.
    ///
    /// `no_hooks` selects the built-in `jj git push` command instead of a
    /// configured command. It does not re-enable pushing when config disables
    /// it.
    #[must_use]
    pub fn resolve_argv(&self, no_hooks: bool) -> Option<Vec<String>> {
        match (self, no_hooks) {
            (Self::Enabled(false), _) => None,
            (_, true) => Some(Self::builtin_push_argv()),
            (_, false) => self.to_argv(),
        }
    }

    fn builtin_push_argv() -> Vec<String> {
        vec!["jj".to_owned(), "git".to_owned(), "push".to_owned()]
    }
}

impl Default for RepoPushConfig {
    fn default() -> Self {
        Self::Enabled(true)
    }
}

pub(crate) fn push_description(push_argv: Option<&[String]>) -> &'static str {
    let Some(argv) = push_argv else {
        return "(pushing disabled)";
    };

    if argv.len() == 3 && argv[0] == "jj" && argv[1] == "git" && argv[2] == "push" {
        "via `jj git push`"
    } else {
        "via configured push command"
    }
}

#[cfg(test)]
mod push_description_tests {
    use super::push_description;

    #[test]
    fn describes_builtin_and_custom_commands_without_rendering_argv() {
        let builtin = vec!["jj".to_owned(), "git".to_owned(), "push".to_owned()];
        let custom = vec!["custom-push".to_owned(), "secret-argument".to_owned()];

        assert_eq!(push_description(Some(&builtin)), "via `jj git push`");
        assert_eq!(
            push_description(Some(&custom)),
            "via configured push command"
        );
        assert_eq!(push_description(None), "(pushing disabled)");
    }
}

#[derive(Debug, Clone, Deserialize, Builder)]
#[serde(rename_all = "camelCase")]
pub struct TitleConfig {
    /// Whether to sync MR titles on every submit, or only once at PR/MR
    /// creation, when a PR/MR has only one revision. Defaults to true.
    #[serde(default = "default_true")]
    pub sync_single_revision: bool,

    /// How to generate the title when a PR/MR has only one revision.
    /// Defaults to "firstCommitFirstLine".
    #[serde(default = "default_single_revision")]
    pub single_revision: TitleFormat,

    /// Whether to sync MR titles on every submit, or only once at PR/MR
    /// creation, when a PR/MR has multiple revisions. Defaults to true.
    #[serde(default = "default_true")]
    pub sync_multiple_revisions: bool,

    /// How to generate the title when a PR/MR has multiple revisions.
    #[serde(default = "default_multiple_revisions")]
    pub multiple_revisions: TitleFormat,
}

fn default_single_revision() -> TitleFormat {
    TitleFormat::FirstRevisionFirstLine
}

fn default_multiple_revisions() -> TitleFormat {
    TitleFormat::BookmarkName
}

impl Default for TitleConfig {
    fn default() -> Self {
        Self {
            sync_single_revision: default_true(),
            single_revision: default_single_revision(),
            sync_multiple_revisions: default_true(),
            multiple_revisions: default_multiple_revisions(),
        }
    }
}

#[derive(Debug, Clone)]
pub enum TitleFormat {
    /// Use the first revision's first line as the title.
    FirstRevisionFirstLine,

    /// Use the first revision's full message as the title.
    FirstRevisionFullMessage,

    /// Use the head revision's first line as the title.
    HeadRevisionFirstLine,

    /// Use the head revision's full message as the title.
    HeadRevisionFullMessage,

    /// Use the bookmark name as the title.
    BookmarkName,

    /// Use a custom template.
    Other(String),
}

impl<'de> Deserialize<'de> for TitleFormat {
    fn deserialize<D>(deserializer: D) -> core::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct TitleFormatVisitor;

        impl Visitor<'_> for TitleFormatVisitor {
            type Value = TitleFormat;

            fn expecting(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
                formatter.write_str("a title format")
            }

            fn visit_str<E>(self, v: &str) -> core::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(match v {
                    "firstRevisionFirstLine" => TitleFormat::FirstRevisionFirstLine,
                    "firstRevisionFullMessage" => TitleFormat::FirstRevisionFullMessage,
                    "headRevisionFirstLine" => TitleFormat::HeadRevisionFirstLine,
                    "headRevisionFullMessage" => TitleFormat::HeadRevisionFullMessage,
                    "bookmarkName" => TitleFormat::BookmarkName,
                    _ => TitleFormat::Other(v.to_owned()),
                })
            }
        }

        deserializer.deserialize_string(TitleFormatVisitor)
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GitLabConfig {
    /// GitLab instance URL (e.g., `https://gitlab.example.com`).
    #[serde(default)]
    pub host: String,

    /// GitLab project ID (e.g., `group/project` or `12345`).
    #[serde(default)]
    pub project: String,

    /// Target project for MRs (if different from project, enables fork
    /// workflow).
    #[serde(default)]
    pub target_project: String,

    /// GitLab Personal Access Token.
    #[serde(default)]
    pub token: String,

    /// If true, jj-vine will create dependencies between pull/merge requests,
    /// requiring that all parent pull/merge requests are merged before the
    /// child pull/merge request can be merged.
    #[serde(default = "default_true")]
    pub create_merge_request_dependencies: bool,
}

impl GitLabConfig {
    /// Get the project where MRs target.
    #[must_use]
    pub fn target_project(&self) -> &str {
        if self.target_project.is_empty() {
            &self.project
        } else {
            &self.target_project
        }
    }

    /// Get the project where branches are pushed.
    #[must_use]
    pub fn source_project(&self) -> &str {
        &self.project
    }

    /// Check if this is a fork workflow (target differs from source).
    #[must_use]
    pub fn is_fork_workflow(&self) -> bool {
        self.target_project() != self.project
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitHubConfig {
    /// GitHub API URL (e.g., `https://api.github.com` or `https://github.example.com/api/v3`).
    #[serde(default)]
    pub host: String,

    /// GitHub repository in `owner/repo` format.
    #[serde(default)]
    pub project: String,

    /// Target repository for PRs (if different from project, enables fork
    /// workflow).
    #[serde(default)]
    pub target_project: String,

    /// GitHub Personal Access Token, as a literal string. Takes precedence
    /// over `tokenCommand` when both are set.
    #[serde(default)]
    pub token: String,

    /// A command whose stdout supplies the GitHub token, as a full argv whose
    /// first element is the binary. Used only when `token` is empty. The
    /// trimmed stdout becomes the token. A non-zero exit or empty output is an
    /// error. The command must be non-interactive: it gets null stdin and, on
    /// Unix, no controlling terminal. Processes remaining in its Unix process
    /// group or Windows job are killed when it exits. A Unix descendant that
    /// starts another session or group can escape cleanup.
    #[serde(default)]
    pub token_command: Vec<String>,
    /// Register submitted pull requests as GitHub-native stacks with
    /// `gh-stack`. Defaults to true, including when `[github]` is absent; set
    /// false to skip stack linking after GitHub submits.
    #[serde(default = "default_true")]
    pub link_stack: bool,
}

impl Default for GitHubConfig {
    fn default() -> Self {
        Self {
            host: String::new(),
            project: String::new(),
            target_project: String::new(),
            token: String::new(),
            token_command: Vec::new(),
            link_stack: true,
        }
    }
}

/// Maximum wall-clock time to wait for a `tokenCommand` helper.
pub(crate) const TOKEN_COMMAND_TIMEOUT: core::time::Duration = core::time::Duration::from_secs(10);

impl GitHubConfig {
    /// Get the repository where PRs target.
    #[must_use]
    pub fn target_project(&self) -> &str {
        if self.target_project.is_empty() {
            &self.project
        } else {
            &self.target_project
        }
    }

    /// Get the repository where branches are pushed.
    #[must_use]
    pub fn source_project(&self) -> &str {
        &self.project
    }

    /// Check if this is a fork workflow (target differs from source).
    #[must_use]
    pub fn is_fork_workflow(&self) -> bool {
        self.target_project() != self.project
    }

    /// Resolve the GitHub token from a literal or a configured command. A
    /// non-empty literal wins and is trimmed; otherwise the command's trimmed
    /// stdout is used. At most the first 1 MiB of stdout is kept; later bytes
    /// are dropped, and a truncated multibyte character fails UTF-8 validation.
    /// The command must finish within 10 seconds of this call, `PATH` lookup
    /// included.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Config`] if neither `token` nor `tokenCommand` is
    /// set, the command binary is not found in `PATH`, the command does not
    /// finish in time, exits with a non-zero status, or prints output that is
    /// not UTF-8 or is empty after trimming. Returns [`Error::Io`] if the
    /// command cannot be spawned, contained, or reaped, or (on Unix) if a
    /// caught cancellation signal cut the run short. No error includes the
    /// command's stdout or stderr.
    pub fn resolved_token(&self) -> Result<String> {
        self.resolved_token_with_timeout(TOKEN_COMMAND_TIMEOUT)
    }

    /// Resolve the token with a caller-supplied timeout for deterministic
    /// tests.
    pub(crate) fn resolved_token_with_timeout(
        &self,
        timeout: core::time::Duration,
    ) -> Result<String> {
        self.resolve_token(timeout, None, |bin| which::which(bin).ok())
    }

    /// Resolve the token for a phase that must end by `phase_deadline`. As
    /// [`GitHubConfig::resolved_token`], but the command must also finish by
    /// `phase_deadline`, and is not spawned once it has passed.
    pub(crate) fn resolved_token_by_deadline(
        &self,
        phase_deadline: std::time::Instant,
    ) -> Result<String> {
        self.resolve_token(TOKEN_COMMAND_TIMEOUT, Some(phase_deadline), |bin| {
            which::which(bin).ok()
        })
    }

    /// Resolve the token. The command's `timeout` starts at this call, before
    /// `find_binary` searches `PATH`, so a slow lookup counts against it. The
    /// command must finish by the earlier of that limit and `phase_deadline`,
    /// and is not spawned once that instant has passed.
    fn resolve_token(
        &self,
        timeout: core::time::Duration,
        phase_deadline: Option<std::time::Instant>,
        find_binary: impl FnOnce(&str) -> Option<PathBuf>,
    ) -> Result<String> {
        let start = std::time::Instant::now();
        let literal = self.token.trim();
        if !literal.is_empty() {
            return Ok(literal.to_owned());
        }

        let Some((bin, args)) = self.token_command.split_first() else {
            return Err(ConfigSnafu {
                message: "github.token or github.tokenCommand is required when forge is github"
                    .to_owned(),
            }
            .build());
        };

        let timeout_end = start
            .checked_add(timeout)
            .ok_or_else(|| std::io::Error::other("subprocess timeout is too large"))?;
        let deadline = phase_deadline.map_or(timeout_end, |phase| phase.min(timeout_end));
        let budget = deadline.saturating_duration_since(start);

        let bin_path = find_binary(bin).ok_or_else(|| {
            ConfigSnafu {
                message: "github.tokenCommand binary not found in PATH".to_owned(),
            }
            .build()
        })?;

        let mut command = std::process::Command::new(&bin_path);
        command.args(args);
        let Some(output) = crate::process::output_by_deadline(command, deadline)? else {
            return Err(ConfigSnafu {
                message: format!("github.tokenCommand timed out after {budget:?}"),
            }
            .build());
        };
        if !output.status.success() {
            return Err(ConfigSnafu {
                message: format!("github.tokenCommand failed with {}", output.status),
            }
            .build());
        }

        let raw = String::from_utf8(output.stdout).map_err(|_| {
            ConfigSnafu {
                message: "github.tokenCommand produced non-UTF-8 output".to_owned(),
            }
            .build()
        })?;
        let token = raw.trim().to_owned();
        if token.is_empty() {
            return Err(ConfigSnafu {
                message: "github.tokenCommand produced empty output".to_owned(),
            }
            .build());
        }
        Ok(token)
    }
}

/// Not exactly documented, but the default repository setting for Forgejo is:
/// ```go
/// WorkInProgressPrefixes: []string{"WIP:", "[WIP]"}
/// ```
fn default_wip_prefix() -> String {
    "WIP: ".to_owned()
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ForgejoConfig {
    /// Forgejo/Gitea instance URL (e.g., `https://codeberg.org`).
    #[serde(default)]
    pub host: String,

    /// Repository in `owner/repo` format.
    #[serde(default)]
    pub project: String,

    /// Target repository for PRs (if different from project, enables fork
    /// workflow).
    #[serde(default)]
    pub target_project: String,

    /// API access token.
    #[serde(default)]
    pub token: String,

    /// Prefix for WIP pull/merge requests.
    #[serde(default = "default_wip_prefix")]
    pub wip_prefix: String,
}

impl ForgejoConfig {
    /// Get the repository where PRs target.
    #[must_use]
    pub fn target_project(&self) -> &str {
        if self.target_project.is_empty() {
            &self.project
        } else {
            &self.target_project
        }
    }

    /// Get the repository where branches are pushed.
    #[must_use]
    pub fn source_project(&self) -> &str {
        &self.project
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct AzureDevOpsConfig {
    /// Azure DevOps instance URL (e.g., `https://dev.azure.com`).
    #[serde(default)]
    pub host: String,

    #[serde(default)]
    pub project: String,

    #[serde(default)]
    pub target_project: String,

    #[serde(default)]
    pub token: String,

    #[serde(default)]
    pub source_repository_name: Option<String>,

    #[serde(default)]
    pub target_repository_name: Option<String>,

    #[serde(default)]
    pub source_repository_id: Option<String>,

    #[serde(default)]
    pub target_repository_id: Option<String>,

    #[serde(default)]
    pub vssps_host: String,
}

impl AzureDevOpsConfig {
    #[must_use]
    pub fn source_project_id(&self) -> &str {
        &self.project
    }

    #[must_use]
    pub fn target_project_id(&self) -> &str {
        if self.target_project.is_empty() {
            &self.project
        } else {
            &self.target_project
        }
    }

    #[must_use]
    pub fn target_repository_name(&self) -> Option<&str> {
        if self.target_repository_name.is_none() {
            self.source_repository_name.as_deref()
        } else {
            self.target_repository_name.as_deref()
        }
    }

    #[must_use]
    pub fn target_repository_id(&self) -> Option<&str> {
        if self.target_repository_id.is_none() {
            self.source_repository_id.as_deref()
        } else {
            self.target_repository_id.as_deref()
        }
    }
}

/// Where to place the stack visualization in a pull/merge request description.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StackPlacement {
    /// Place the stack visualization before user content.
    Top,

    /// Place the stack visualization after user content. This is the default.
    #[default]
    Bottom,
}

fn default_description_single_revision() -> DescriptionMode {
    DescriptionMode::NotFirstLine
}

fn default_description_multiple_revisions() -> DescriptionMode {
    DescriptionMode::CommitListFull
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DescriptionConfig {
    /// Whether to enable or disable description generation entirely.
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Whether to sync the description of a pull/merge request every time the
    /// bookmark is submitted. Defaults to false.
    #[serde(default)]
    pub sync: bool,

    /// How to handle the description for pull/merge
    /// requests, when there is only one revision in the pull/merge request. By
    /// default, includes the first line of the commit message.
    #[serde(default = "default_description_single_revision")]
    pub single_revision: DescriptionMode,

    /// How to handle the description for pull/merge
    /// requests, when there are multiple revisions in the pull/merge request.
    /// By default, includes the full commit messages of all revisions.
    #[serde(default = "default_description_multiple_revisions")]
    pub multiple_revisions: DescriptionMode,

    /// How to render the description for different types of pull/merge request
    /// stacks.
    #[serde(default)]
    pub diagram: DescriptionDiagramConfig,

    /// Where to place the stack visualization in the description (defaults to
    /// bottom).
    #[serde(default)]
    pub placement: StackPlacement,
}

impl Default for DescriptionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sync: false,
            diagram: DescriptionDiagramConfig::default(),
            placement: StackPlacement::default(),
            single_revision: DescriptionMode::NotFirstLine,
            multiple_revisions: DescriptionMode::CommitListFull,
        }
    }
}

#[derive(Debug, Clone)]
pub enum DescriptionMode {
    /// Do not render a description.
    None,

    /// Render the message of the head commit in the branch, but
    /// not the first line (because that is already used for the title).
    NotFirstLine,

    /// Render the full message of the head commit in the branch.
    FullMessage,

    /// Render a list of all commits in the branch, with their
    /// hashes and the first line of each commit message.
    CommitListFirstLine,

    /// Render a list of all commits in the branch, with their
    /// hashes and full commit messages.
    CommitListFull,

    /// Include the contents of a file at the given path as the description.
    File(String),
}

impl<'de> Deserialize<'de> for DescriptionMode {
    fn deserialize<D>(deserializer: D) -> core::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct DescriptionModeVisitor;
        impl Visitor<'_> for DescriptionModeVisitor {
            type Value = DescriptionMode;
            fn expecting(&self, formatter: &mut core::fmt::Formatter) -> core::fmt::Result {
                formatter.write_str("a description mode")
            }

            fn visit_str<E>(self, v: &str) -> core::result::Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                match v {
                    "none" => Ok(DescriptionMode::None),
                    "notFirstLine" => Ok(DescriptionMode::NotFirstLine),
                    "fullMessage" => Ok(DescriptionMode::FullMessage),
                    "commitListFirstLine" => Ok(DescriptionMode::CommitListFirstLine),
                    "commitListFull" => Ok(DescriptionMode::CommitListFull),
                    mode if mode.starts_with("file(") && mode.ends_with(')') => {
                        Ok(DescriptionMode::File(
                            mode.trim_start_matches("file(")
                                .trim_end_matches(')')
                                .to_owned(),
                        ))
                    }
                    _ => Err(E::custom(format!("invalid description mode: {v}"))),
                }
            }
        }

        deserializer.deserialize_string(DescriptionModeVisitor)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DescriptionDiagramConfig {
    /// How to render a single pull/merge request, without any parents or
    /// children besides the trunk. Defaults to not rendering a description.
    pub single: DescriptionDiagramFormat,

    /// How to render a linear stack of MRs.
    /// Defaults to a linear numbered list.
    pub linear: DescriptionDiagramFormat,

    /// How to render a tree of MRs, where two MRs merge into a common parent.
    /// Defaults to a linear numbered list.
    pub tree: DescriptionDiagramFormat,

    /// How to render a complex graph of MRs, where two MRs merge into a common
    /// parent, or any pull/merge request has multiple parents.
    /// Defaults to a linear numbered list.
    pub complex: DescriptionDiagramFormat,
}

impl Default for DescriptionDiagramConfig {
    fn default() -> Self {
        Self {
            single: DescriptionDiagramFormat::None,
            linear: DescriptionDiagramFormat::Linear,
            tree: DescriptionDiagramFormat::Linear,
            complex: DescriptionDiagramFormat::Linear,
        }
    }
}

/// Default GitHub API URL when no host is configured or derived.
const GITHUB_DEFAULT_API_HOST: &str = "https://api.github.com";

/// Names (`jj-vine.github.<key>`) whose effective value comes from the repo
/// or workspace layer. Such values are explicit for this clone; values from
/// the user/global layer are not. Only names and layer sources are read, so
/// config values such as tokens never enter this output. Returns `None` when
/// the layer sources cannot be read.
fn clone_layer_keys(jj: &Jujutsu) -> Option<Vec<String>> {
    let output = jj
        .exec([
            "config",
            "list",
            "--template",
            r#"name ++ "\t" ++ source ++ "\n""#,
            "jj-vine.github",
        ])
        .ok()?;
    Some(
        output
            .stdout
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .filter(|(_, source)| matches!(*source, "repo" | "workspace"))
            .map(|(name, _)| name.to_owned())
            .collect(),
    )
}

/// A non-empty effective value from the repo or workspace layer takes
/// precedence over clone-derived values. When the layer sources are unknown,
/// every non-empty value is kept, so a failed lookup never replaces a value
/// that may be explicit.
fn clone_layer_nonempty(clone_keys: Option<&[String]>, key: &str, value: &str) -> bool {
    !value.is_empty()
        && clone_keys.is_none_or(|keys| {
            keys.iter()
                .any(|name| name.strip_prefix("jj-vine.github.") == Some(key))
        })
}

/// Whether a configured API host uses HTTPS, so the token never travels
/// over plaintext.
pub(crate) fn is_https_host(host: &str) -> bool {
    host.get(..8)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https://"))
}

impl Config {
    /// Load configuration from jj config.
    pub fn load(repo_path: impl Into<PathBuf>) -> Result<Self> {
        Self::load_with(&Jujutsu::new(repo_path)?)
    }

    /// Load configuration through an existing Jujutsu instance. Tests use
    /// this with an isolated config file without changing the public API.
    fn load_with(jj: &Jujutsu) -> Result<Self> {
        let output = jj.exec(["config", "list"])?;

        let toml_value: toml::Value = toml::from_str(&output.stdout).map_err(|e| {
            ConfigSnafu {
                message: format!("Failed to parse config as TOML: {e}"),
            }
            .build()
        })?;

        let jj_vine_value = toml_value.get("jj-vine").ok_or_else(|| {
            ConfigSnafu {
                message: "Missing required config section: jj-vine".to_owned(),
            }
            .build()
        })?;

        let mut config: Config = jj_vine_value.clone().try_into().map_err(|e| {
            ConfigSnafu {
                message: format!("Failed to parse jj-vine config: {e}"),
            }
            .build()
        })?;

        // Clone-explicit values win; derived values supersede global ones, and
        // empty values derive. A plaintext Enterprise remote needs a
        // clone-explicit HTTPS host. A derived target also needs the effective
        // host and source project to match the remote's. A remote that fetches
        // another repository from another API host needs a clone-explicit
        // HTTPS host and target, because one API host serves both projects.
        if config.forge == ForgeType::GitHub {
            let clone_keys = clone_layer_keys(jj);
            let clone_keys = clone_keys.as_deref();
            let project_set = clone_layer_nonempty(clone_keys, "project", &config.github.project);
            let host_set = clone_layer_nonempty(clone_keys, "host", &config.github.host);
            let target_set =
                clone_layer_nonempty(clone_keys, "targetProject", &config.github.target_project);
            let https_host_set = host_set && is_https_host(&config.github.host);
            if (!project_set || !https_host_set || !target_set)
                && let Some(detected) =
                    crate::remote::detect_project(jj, &config.remote_name, ForgeType::GitHub)
            {
                // A different effective host or source project would pair the
                // fetch repository with another server or another source.
                let source_matches = !project_set
                    || config
                        .github
                        .project
                        .eq_ignore_ascii_case(&detected.project);
                let derived_target = match (&detected.host, detected.fetch_target) {
                    (ApiHost::Derived(host), FetchTarget::Project(target))
                        if source_matches
                            && (!host_set || same_api_host(&config.github.host, host)) =>
                    {
                        Some(target)
                    }
                    (_, FetchTarget::OtherHost) if !(https_host_set && target_set) => {
                        return ConfigSnafu {
                            message: format!(
                                "the '{}' remote fetches from another repository on a \
                                 different API host than it pushes to, so github.host and \
                                 github.targetProject are not derived; set \
                                 jj-vine.github.host to an HTTPS API URL and \
                                 jj-vine.github.targetProject in the repository or workspace \
                                 config",
                                config.remote_name
                            ),
                        }
                        .fail();
                    }
                    _ => None,
                };
                if !project_set {
                    config.github.project = detected.project;
                }
                match detected.host {
                    ApiHost::Derived(host) if !host_set => config.github.host = host,
                    // An SSH alias remote has no known host; keep the configured one.
                    ApiHost::Derived(_) | ApiHost::Unknown => {}
                    ApiHost::PlaintextEnterprise if https_host_set => {}
                    ApiHost::PlaintextEnterprise => {
                        return ConfigSnafu {
                            message: format!(
                                "the '{}' remote is a GitHub Enterprise remote over plain HTTP, \
                                 so github.host is not derived and the API token is not sent \
                                 over it; set jj-vine.github.host in the repository or \
                                 workspace config to the Enterprise HTTPS API URL",
                                config.remote_name
                            ),
                        }
                        .fail();
                    }
                }
                // `detect_project` sets a target only when it differs from the
                // push project, ignoring ASCII case.
                if !target_set && let Some(target) = derived_target {
                    config.github.target_project = target;
                }
            }
            if config.github.host.is_empty() {
                GITHUB_DEFAULT_API_HOST.clone_into(&mut config.github.host);
            }
        }

        config.validate()?;

        Ok(config)
    }

    /// Resolve the base branch used by root MRs and stack labels.
    pub fn root_base_branch<'a>(&'a self, jj: &'a Jujutsu) -> Result<&'a str> {
        match self.default_base_branch.as_deref() {
            Some(branch) => Ok(branch),
            None => jj.default_branch(),
        }
    }

    pub fn validate(&self) -> Result<()> {
        match self.forge {
            ForgeType::GitLab => crate::forge::gitlab::validate_config(self),
            ForgeType::GitHub => crate::forge::github::validate_config(self),
            ForgeType::Forgejo => crate::forge::forgejo::validate_config(self),
            ForgeType::AzureDevOps => crate::forge::azure::validate_config(self),
        }?;

        if matches!(&self.push, RepoPushConfig::Command(argv) if argv.is_empty()) {
            return Err(ConfigSnafu {
                message: "jj-vine.push must be a boolean or a non-empty command array".to_owned(),
            }
            .build());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;
    use crate::jj::ISOLATED_TEST_CONFIG;

    fn isolated_jj(repo_path: &Path) -> Result<Jujutsu> {
        let config_path = repo_path
            .parent()
            .expect("test repo always has a parent temp dir")
            .join(ISOLATED_TEST_CONFIG);
        Jujutsu::new_isolated(repo_path, config_path)
    }

    fn load_isolated(repo_path: &Path) -> Result<Config> {
        load_with(&isolated_jj(repo_path)?)
    }

    /// Load using a caller-supplied Jujutsu instance.
    fn load_with(jj: &Jujutsu) -> Result<Config> {
        Config::load_with(jj)
    }
    fn create_test_repo() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");
        std::fs::write(temp_dir.path().join(ISOLATED_TEST_CONFIG), "")
            .expect("Failed to write isolated config");
        let repo_path = temp_dir.path().join("repo");
        std::fs::create_dir_all(&repo_path).expect("Failed to create repo dir");

        let jj = isolated_jj(&repo_path).expect("Failed to create Jujutsu instance");
        jj.exec(["git", "init", "--colocate"])
            .expect("Failed to init jj repo");

        (temp_dir, repo_path)
    }

    fn set_repo_config(repo_path: &Path, key: &str, value: &str) {
        isolated_jj(repo_path)
            .expect("Failed to create Jujutsu instance")
            .exec(["config", "set", "--repo", key, value])
            .expect("Failed to set repo config");
    }

    fn add_git_remote(repo_path: &Path, name: &str, url: &str) {
        isolated_jj(repo_path)
            .expect("Failed to create Jujutsu instance")
            .exec(["git", "remote", "add", name, url])
            .expect("Failed to add git remote");
    }

    fn seed_github_config(repo_path: &Path) {
        set_repo_config(repo_path, "jj-vine.forge", "github");
        set_repo_config(repo_path, "jj-vine.github.token", "test-token");
    }

    #[test]
    fn derive_project_and_host_when_unset() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        add_git_remote(&repo_path, "origin", "git@github.com:owner/repo.git");

        let config = load_isolated(&repo_path).expect("load config from GitHub remote");

        assert_eq!(config.github.project, "owner/repo");
        assert_eq!(config.github.host, "https://api.github.com");
    }

    #[test]
    fn derive_missing_field_independently() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        set_repo_config(&repo_path, "jj-vine.github.project", "configured/repo");
        add_git_remote(&repo_path, "origin", "git@github.com:owner/repo.git");

        let config = load_isolated(&repo_path).expect("load config from GitHub remote");

        assert_eq!(config.github.project, "configured/repo");
        assert_eq!(config.github.host, "https://api.github.com");
    }

    #[test]
    fn derive_supersedes_global_project_and_host() {
        let (temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        std::fs::write(
            temp.path().join(ISOLATED_TEST_CONFIG),
            "[jj-vine.github]\nproject = \"stale/global\"\nhost = \"https://stale.example/api/v3\"\n",
        )
        .expect("write user-level GitHub values");
        add_git_remote(&repo_path, "origin", "git@github.com:owner/repo.git");

        let config = load_isolated(&repo_path).expect("load config from GitHub remote");

        assert_eq!(config.github.project, "owner/repo");
        assert_eq!(config.github.host, "https://api.github.com");
    }

    #[test]
    fn derive_detection_failure_falls_back_to_validate_error() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);

        let result = load_isolated(&repo_path);
        let Err(Error::Config { message, .. }) = result else {
            panic!("Expected Config error, got: {result:?}");
        };

        assert!(
            message.contains("could not be derived from the 'origin' remote"),
            "validation error must explain remote detection failure: {message}"
        );
    }

    #[test]
    fn derive_fork_workflow_fence_errors() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        add_git_remote(&repo_path, "origin", "git@github.com:owner/fork.git");
        add_git_remote(&repo_path, "upstream", "git@github.com:owner/canonical.git");

        let result = load_isolated(&repo_path);
        let Err(Error::Config { message, .. }) = result else {
            panic!("Expected Config error, got: {result:?}");
        };

        assert!(
            message.contains("could not be derived from the 'origin' remote"),
            "fork workflow must skip derivation and explain the missing project: {message}"
        );
    }

    #[test]
    fn derive_fork_beside_origin_fence_errors() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        set_repo_config(&repo_path, "jj-vine.remoteName", "fork");
        add_git_remote(&repo_path, "origin", "git@github.com:owner/canonical.git");
        add_git_remote(&repo_path, "fork", "git@github.com:person/fork.git");

        let error = load_isolated(&repo_path).expect_err("fork layout skips derivation");

        assert!(
            error
                .to_string()
                .contains("could not be derived from the 'fork' remote"),
            "fork layout must explain the missing project: {error}"
        );
    }

    #[test]
    fn derive_respects_configured_remote_name() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        set_repo_config(&repo_path, "jj-vine.remoteName", "upstream");
        add_git_remote(
            &repo_path,
            "upstream",
            "git@github.example.com:owner/canonical.git",
        );

        let config = load_isolated(&repo_path).expect("load config from configured remote");

        assert_eq!(config.github.project, "owner/canonical");
        assert_eq!(config.github.host, "https://github.example.com/api/v3");
    }

    #[test]
    fn derive_enterprise_host_supersedes_global_host() {
        let (temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        std::fs::write(
            temp.path().join(ISOLATED_TEST_CONFIG),
            "[jj-vine.github]\nhost = \"https://stale.example/api/v3\"\n",
        )
        .expect("write user-level GitHub host");
        add_git_remote(
            &repo_path,
            "origin",
            "https://github.example.com:8443/owner/repo.git",
        );

        let config = load_isolated(&repo_path).expect("load config from Enterprise remote");

        assert_eq!(config.github.project, "owner/repo");
        assert_eq!(config.github.host, "https://github.example.com:8443/api/v3");
    }

    /// A plaintext Enterprise remote derives no host, and loading fails rather
    /// than send the token to the public API or an `http://` host. Neither a
    /// global host nor an explicit `http://` host overrides it. The error
    /// names no URL.
    #[test]
    fn derive_skips_http_enterprise_remote() {
        for (global_host, repo_host) in [
            (None, None),
            (Some("https://github.example.com/api/v3"), None),
            (None, Some("http://github.example.com/api/v3")),
        ] {
            let (temp, repo_path) = create_test_repo();
            seed_github_config(&repo_path);
            set_repo_config(&repo_path, "jj-vine.github.project", "configured/repo");
            if let Some(host) = global_host {
                std::fs::write(
                    temp.path().join(ISOLATED_TEST_CONFIG),
                    format!("[jj-vine.github]\nhost = \"{host}\"\n"),
                )
                .expect("write user-level GitHub host");
            }
            if let Some(host) = repo_host {
                set_repo_config(&repo_path, "jj-vine.github.host", host);
            }
            add_git_remote(
                &repo_path,
                "origin",
                "http://user:s3cret-token@github.example.com/owner/repo.git",
            );

            let result = load_isolated(&repo_path);
            let Err(Error::Config { message, .. }) = result else {
                panic!("Expected Config error for {global_host:?}/{repo_host:?}, got: {result:?}");
            };

            assert!(
                message.contains("GitHub Enterprise remote over plain HTTP"),
                "{message}"
            );
            assert!(!message.contains("github.example.com"), "{message}");
            assert!(!message.contains("s3cret-token"), "{message}");
        }
    }

    /// An explicit repository HTTPS host is the safe override for a plaintext
    /// Enterprise remote; the project still derives from the remote.
    #[test]
    fn derive_http_enterprise_remote_uses_explicit_https_host() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        set_repo_config(
            &repo_path,
            "jj-vine.github.host",
            "https://github.example.com/api/v3",
        );
        add_git_remote(
            &repo_path,
            "origin",
            "http://github.example.com/owner/repo.git",
        );

        let config = load_isolated(&repo_path).expect("load config with explicit HTTPS host");

        assert_eq!(config.github.host, "https://github.example.com/api/v3");
        assert_eq!(config.github.project, "owner/repo");
    }

    /// An SSH config alias names no real host: the project is derived, the
    /// global host stays.
    #[test]
    fn derive_ssh_alias_keeps_global_host() {
        let (temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        std::fs::write(
            temp.path().join(ISOLATED_TEST_CONFIG),
            "[jj-vine.github]\nhost = \"https://github.example.com/api/v3\"\n",
        )
        .expect("write user-level GitHub host");
        add_git_remote(&repo_path, "origin", "git@github.com-work:owner/repo.git");

        let config = load_isolated(&repo_path).expect("load config from alias remote");

        assert_eq!(config.github.project, "owner/repo");
        assert_eq!(config.github.host, "https://github.example.com/api/v3");
    }

    /// Unknown layer sources keep every non-empty value, so a failed source
    /// lookup never lets the remote replace a possibly explicit value.
    #[test]
    fn unknown_layer_sources_keep_nonempty_values() {
        assert!(clone_layer_nonempty(None, "project", "configured/repo"));
        assert!(!clone_layer_nonempty(None, "project", ""));
        assert!(!clone_layer_nonempty(
            Some(&[][..]),
            "project",
            "global/repo"
        ));
    }

    #[test]
    fn derive_does_not_fill_non_github_config() {
        let (_temp, repo_path) = create_test_repo();
        set_repo_config(&repo_path, "jj-vine.forge", "gitlab");
        set_repo_config(&repo_path, "jj-vine.gitlab.host", "https://gitlab.com");
        set_repo_config(&repo_path, "jj-vine.gitlab.project", "group/repo");
        set_repo_config(&repo_path, "jj-vine.gitlab.token", "test-token");
        add_git_remote(&repo_path, "origin", "git@github.com:owner/repo.git");

        let config = load_isolated(&repo_path).expect("load GitLab config");

        assert!(config.github.project.is_empty());
        assert!(config.github.host.is_empty());
    }

    #[test]
    fn derive_explicit_empty_project_collapses_to_absent() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        set_repo_config(&repo_path, "jj-vine.github.project", "");
        add_git_remote(&repo_path, "origin", "git@github.com:owner/repo.git");

        let config = load_isolated(&repo_path).expect("load config from GitHub remote");

        assert_eq!(config.github.project, "owner/repo");
    }
    #[test]
    fn derive_explicit_empty_host_collapses_to_absent() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        set_repo_config(&repo_path, "jj-vine.github.project", "configured/repo");
        set_repo_config(&repo_path, "jj-vine.github.host", "");
        add_git_remote(&repo_path, "origin", "git@github.com:owner/remote.git");

        let config = load_isolated(&repo_path).expect("load config from GitHub remote");

        assert_eq!(config.github.project, "configured/repo");
        assert_eq!(config.github.host, "https://api.github.com");
    }

    #[test]
    fn derive_does_not_change_target_project() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        add_git_remote(&repo_path, "origin", "git@github.com:owner/repo.git");

        let config = load_isolated(&repo_path).expect("load config from GitHub remote");

        assert!(config.github.target_project.is_empty());
    }

    fn set_git_push_url(repo_path: &Path, name: &str, url: &str) {
        isolated_jj(repo_path)
            .expect("Failed to create Jujutsu instance")
            .exec(["git", "remote", "set-url", name, "--push", url])
            .expect("Failed to set push URL");
    }

    /// A remote that fetches the canonical repository and pushes to
    /// another one on the same host is a fork workflow. A global
    /// `targetProject` is superseded, like global `project` and `host`.
    #[test]
    fn derive_target_project_from_distinct_fetch_url() {
        let (temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        std::fs::write(
            temp.path().join(ISOLATED_TEST_CONFIG),
            "[jj-vine.github]\ntargetProject = \"stale/global\"\n",
        )
        .expect("write user-level GitHub target");
        add_git_remote(&repo_path, "origin", "https://github.com/owner/repo.git");
        set_git_push_url(&repo_path, "origin", "git@github.com:person/fork.git");

        let config = load_isolated(&repo_path).expect("load config from fork remote");

        assert_eq!(config.github.source_project(), "person/fork");
        assert_eq!(config.github.target_project(), "owner/repo");
        assert_eq!(config.github.host, "https://api.github.com");
        assert!(config.github.is_fork_workflow());
    }

    /// Repo- and workspace-explicit `targetProject` win over the fetch URL.
    #[test]
    fn derive_target_project_keeps_clone_explicit_value() {
        for set_config in [set_repo_config, set_workspace_config] {
            let (_temp, repo_path) = create_test_repo();
            seed_github_config(&repo_path);
            set_config(
                &repo_path,
                "jj-vine.github.targetProject",
                "explicit/target",
            );
            add_git_remote(&repo_path, "origin", "https://github.com/owner/repo.git");
            set_git_push_url(&repo_path, "origin", "git@github.com:person/fork.git");

            let config = load_isolated(&repo_path).expect("load config with explicit target");

            assert_eq!(config.github.project, "person/fork");
            assert_eq!(config.github.target_project(), "explicit/target");
        }
    }

    /// An explicit host other than the one both URLs resolve to keeps the
    /// target unset: the fetch URL names a repository on another server.
    #[test]
    fn derive_target_project_skipped_for_other_explicit_host() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        set_repo_config(
            &repo_path,
            "jj-vine.github.host",
            "https://ghe.example/api/v3",
        );
        add_git_remote(&repo_path, "origin", "https://github.com/owner/repo.git");
        set_git_push_url(&repo_path, "origin", "git@github.com:person/fork.git");

        let config = load_isolated(&repo_path).expect("load config with explicit host");

        assert_eq!(config.github.project, "person/fork");
        assert!(config.github.target_project.is_empty());
    }

    /// A remote that fetches another repository from another API
    /// host fails to load rather than route the token for both projects to
    /// one derived host. The error names no URL.
    #[test]
    fn derive_cross_host_fetch_url_fails_closed() {
        for (repo_host, repo_target) in [
            (None, None),
            (Some("https://api.github.com"), None),
            (None, Some("owner/repo")),
            (Some("http://github.example.com/api/v3"), Some("owner/repo")),
        ] {
            let (_temp, repo_path) = create_test_repo();
            seed_github_config(&repo_path);
            if let Some(host) = repo_host {
                set_repo_config(&repo_path, "jj-vine.github.host", host);
            }
            if let Some(target) = repo_target {
                set_repo_config(&repo_path, "jj-vine.github.targetProject", target);
            }
            add_git_remote(
                &repo_path,
                "origin",
                "https://github.example.com/owner/repo.git",
            );
            set_git_push_url(&repo_path, "origin", "git@github.com:person/fork.git");

            let result = load_isolated(&repo_path);
            let Err(Error::Config { message, .. }) = result else {
                panic!("Expected Config error for {repo_host:?}/{repo_target:?}, got: {result:?}");
            };

            assert!(message.contains("different API host"), "{message}");
            assert!(!message.contains("github.example.com"), "{message}");
        }
    }

    /// A cross-host pair loads once the clone names an HTTPS host and target
    /// explicitly; nothing is derived for the pair beyond the push project.
    #[test]
    fn derive_cross_host_fetch_url_uses_explicit_https_host_and_target() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        set_repo_config(&repo_path, "jj-vine.github.host", "https://api.github.com");
        set_repo_config(&repo_path, "jj-vine.github.targetProject", "owner/repo");
        add_git_remote(
            &repo_path,
            "origin",
            "https://github.example.com/owner/repo.git",
        );
        set_git_push_url(&repo_path, "origin", "git@github.com:person/fork.git");

        let config = load_isolated(&repo_path).expect("load config with explicit host");

        assert_eq!(config.github.host, "https://api.github.com");
        assert_eq!(config.github.project, "person/fork");
        assert_eq!(config.github.target_project(), "owner/repo");
    }

    /// Distinct fetch and push repositories on one Enterprise host derive
    /// both projects and that host.
    #[test]
    fn derive_same_enterprise_host_fetch_and_push() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        add_git_remote(
            &repo_path,
            "origin",
            "https://github.example.com/owner/repo.git",
        );
        set_git_push_url(
            &repo_path,
            "origin",
            "git@github.example.com:person/fork.git",
        );

        let config = load_isolated(&repo_path).expect("load config from same-host remote");

        assert_eq!(config.github.host, "https://github.example.com/api/v3");
        assert_eq!(config.github.project, "person/fork");
        assert_eq!(config.github.target_project(), "owner/repo");
    }

    /// An explicit host naming the derived endpoint with other hostname case,
    /// an explicit default port, or a trailing slash still derives the target.
    #[test]
    fn derive_target_project_for_matching_explicit_host() {
        for host in [
            "https://api.github.com",
            "https://API.GitHub.com/",
            "https://api.github.com:443",
        ] {
            let (_temp, repo_path) = create_test_repo();
            seed_github_config(&repo_path);
            set_repo_config(&repo_path, "jj-vine.github.host", host);
            add_git_remote(&repo_path, "origin", "https://github.com/owner/repo.git");
            set_git_push_url(&repo_path, "origin", "git@github.com:person/fork.git");

            let config = load_isolated(&repo_path).expect("load config with matching host");

            assert_eq!(config.github.host, host);
            assert_eq!(config.github.project, "person/fork", "{host}");
            assert_eq!(config.github.target_project(), "owner/repo", "{host}");
        }
    }

    /// A clone-explicit plain HTTP host is rejected even with an
    /// HTTPS remote, so the token never travels over plaintext.
    #[test]
    fn explicit_http_host_is_rejected() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        set_repo_config(
            &repo_path,
            "jj-vine.github.host",
            "http://github.example.com/api/v3",
        );
        add_git_remote(
            &repo_path,
            "origin",
            "https://github.example.com/owner/repo.git",
        );
        set_git_push_url(
            &repo_path,
            "origin",
            "git@github.example.com:person/fork.git",
        );

        let result = load_isolated(&repo_path);
        let Err(Error::Config { message, .. }) = result else {
            panic!("Expected Config error, got: {result:?}");
        };

        assert!(message.contains("https:// API URL"), "{message}");
    }

    /// A global plain HTTP host is rejected when nothing derives
    /// a replacement.
    #[test]
    fn global_http_host_is_rejected_without_remote() {
        let (temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        set_repo_config(&repo_path, "jj-vine.github.project", "owner/repo");
        std::fs::write(
            temp.path().join(ISOLATED_TEST_CONFIG),
            "[jj-vine.github]\nhost = \"http://github.example.com/api/v3\"\n",
        )
        .expect("write user-level GitHub host");

        let result = load_isolated(&repo_path);
        let Err(Error::Config { message, .. }) = result else {
            panic!("Expected Config error, got: {result:?}");
        };

        assert!(message.contains("https:// API URL"), "{message}");
    }

    /// A clone-explicit source project other than the push URL's keeps the
    /// target unset: the fetch repository is not that project's upstream.
    #[test]
    fn derive_target_project_skipped_for_mismatched_explicit_project() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        set_repo_config(&repo_path, "jj-vine.github.project", "other/source");
        add_git_remote(&repo_path, "origin", "https://github.com/owner/repo.git");
        set_git_push_url(&repo_path, "origin", "git@github.com:person/fork.git");

        let config = load_isolated(&repo_path).expect("load config with explicit project");

        assert_eq!(config.github.project, "other/source");
        assert!(config.github.target_project.is_empty());
    }

    /// A clone-explicit source project that matches the push URL up to ASCII
    /// case still derives the target.
    #[test]
    fn derive_target_project_for_case_matching_explicit_project() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        set_repo_config(&repo_path, "jj-vine.github.project", "Person/Fork");
        add_git_remote(&repo_path, "origin", "https://github.com/owner/repo.git");
        set_git_push_url(&repo_path, "origin", "git@github.com:person/fork.git");

        let config = load_isolated(&repo_path).expect("load config with explicit project");

        assert_eq!(config.github.project, "Person/Fork");
        assert_eq!(config.github.target_project(), "owner/repo");
    }

    /// A fetch URL naming the push repository in other case is no fork.
    #[test]
    fn derive_target_project_skipped_for_case_only_difference() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        add_git_remote(&repo_path, "origin", "https://github.com/Person/Fork.git");
        set_git_push_url(&repo_path, "origin", "git@github.com:person/fork.git");

        let config = load_isolated(&repo_path).expect("load config from case-only remote");

        assert_eq!(config.github.project, "person/fork");
        assert!(config.github.target_project.is_empty());
        assert!(!config.github.is_fork_workflow());
    }

    /// With `upstream`, `fork`, and `origin`, only `upstream` derives.
    #[test]
    fn derive_upstream_beside_fork_and_origin() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        set_repo_config(&repo_path, "jj-vine.remoteName", "upstream");
        add_git_remote(&repo_path, "origin", "git@github.com:person/origin.git");
        add_git_remote(&repo_path, "fork", "git@github.com:person/fork.git");
        add_git_remote(&repo_path, "upstream", "git@github.com:owner/repo.git");

        let config = load_isolated(&repo_path).expect("load config from upstream");

        assert_eq!(config.github.project, "owner/repo");
    }

    #[test]
    fn derive_explicit_host_is_preserved_while_project_derives() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        set_repo_config(
            &repo_path,
            "jj-vine.github.host",
            "https://ghe.example/api/v3",
        );
        add_git_remote(&repo_path, "origin", "git@github.com:owner/repo.git");

        let config = load_isolated(&repo_path).expect("load config from GitHub remote");

        assert_eq!(config.github.host, "https://ghe.example/api/v3");
        assert_eq!(config.github.project, "owner/repo");
    }

    #[test]
    fn derive_gitlab_remote_as_github_is_not_used() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        add_git_remote(&repo_path, "origin", "git@gitlab.com:group/repo.git");

        let result = load_isolated(&repo_path);
        let Err(Error::Config { message, .. }) = result else {
            panic!("Expected Config error, got: {result:?}");
        };

        assert!(message.contains("could not be derived from the 'origin' remote"));
    }

    fn set_workspace_config(repo_path: &Path, key: &str, value: &str) {
        isolated_jj(repo_path)
            .expect("Failed to create Jujutsu instance")
            .exec(["config", "set", "--workspace", key, value])
            .expect("Failed to set workspace config");
    }

    #[test]
    fn derive_preserves_workspace_explicit_values() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        set_workspace_config(&repo_path, "jj-vine.github.project", "workspace/repo");
        set_workspace_config(
            &repo_path,
            "jj-vine.github.host",
            "https://ghe.example/api/v3",
        );
        add_git_remote(&repo_path, "origin", "git@github.com:owner/remote.git");

        let config = load_isolated(&repo_path).expect("load config with workspace values");

        assert_eq!(config.github.project, "workspace/repo");
        assert_eq!(config.github.host, "https://ghe.example/api/v3");
    }

    #[test]
    fn derive_workspace_empty_overrides_repo_value() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        set_repo_config(&repo_path, "jj-vine.github.project", "repo/value");
        set_workspace_config(&repo_path, "jj-vine.github.project", "");
        add_git_remote(&repo_path, "origin", "git@github.com:owner/remote.git");

        let config = load_isolated(&repo_path).expect("load config from GitHub remote");

        // The effective workspace value is empty, so it collapses to absent.
        assert_eq!(config.github.project, "owner/remote");
    }

    #[test]
    fn missing_host_defaults_when_derivation_unavailable() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        set_repo_config(&repo_path, "jj-vine.github.project", "owner/repo");
        add_git_remote(&repo_path, "origin", "git@github.com:person/fork.git");
        add_git_remote(&repo_path, "upstream", "git@github.com:owner/repo.git");

        let config = load_isolated(&repo_path).expect("fenced fork with explicit project");

        assert_eq!(config.github.project, "owner/repo");
        assert_eq!(config.github.host, "https://api.github.com");
    }

    #[test]
    fn missing_host_keeps_global_value_when_derivation_unavailable() {
        let (temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        std::fs::write(
            temp.path().join(ISOLATED_TEST_CONFIG),
            "[jj-vine.github]\nhost = \"https://ghe.example/api/v3\"\n",
        )
        .expect("write user-level GitHub host");
        set_repo_config(&repo_path, "jj-vine.github.project", "owner/repo");

        let config = load_isolated(&repo_path).expect("load without a remote");

        assert_eq!(config.github.host, "https://ghe.example/api/v3");
    }

    /// Verify `JJ_CONFIG` both supplies and replaces the user config layer.
    #[test]
    fn config_isolation_replaces_user_layer() {
        const SEEDED_KEY: &str = r#"jj-vine.branchPrefix = "config-isolation-probe/""#;

        let (temp, repo_path) = create_test_repo();
        let seeded_config = temp.path().join("seeded-user-config.toml");
        std::fs::write(
            &seeded_config,
            "[jj-vine]\nbranchPrefix = \"config-isolation-probe/\"\n",
        )
        .expect("Failed to write seeded config");

        let seeded = Jujutsu::new_isolated(&repo_path, &seeded_config)
            .expect("Failed to create Jujutsu instance");
        let listed = seeded
            .exec(["config", "list"])
            .expect("Failed to list config")
            .stdout;
        assert!(
            listed.contains(SEEDED_KEY),
            "JJ_CONFIG must be honored by the spawned command, so the seeded \
             user-level key is resolved; got: {listed}"
        );

        // The resolved path distinguishes replacement from an additive config.
        let resolved = seeded
            .exec(["config", "path", "--user"])
            .expect("Failed to resolve user config path")
            .stdout;
        assert_eq!(
            Path::new(resolved.trim()),
            seeded_config,
            "JJ_CONFIG must replace the user-level layer"
        );
    }
    #[test]
    fn config_load_missing_required() {
        let (_temp, repo_path) = create_test_repo();

        // Try to load config without setting anything
        let result = load_isolated(&repo_path);
        assert!(result.is_err());

        if let Err(Error::Config { message, .. }) = result {
            assert!(
                message.contains("missing field")
                    || message.contains("gitlab")
                    || message.contains("jj-vine"),
                "Error should mention missing field, got: {message}"
            );
        } else {
            panic!("Expected Config error for missing required field");
        }
    }

    #[test]
    fn config_load_complete() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path).expect("Failed to create Jujutsu instance");
        // Set required config
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.host",
            "https://gitlab.example.com",
        ])
        .expect("Failed to set config");

        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.project",
            "my-group/my-project",
        ])
        .expect("Failed to set config");

        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.token",
            "glpat-test123",
        ])
        .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.forge", "gitlab"])
            .expect("Failed to set config");

        // Load config
        let config = load_isolated(&repo_path).expect("Failed to load config");

        assert_eq!(config.gitlab.host, "https://gitlab.example.com".to_owned());
        assert_eq!(config.gitlab.project, "my-group/my-project".to_owned());
        assert_eq!(config.gitlab.token, "glpat-test123".to_owned());
        assert_eq!(config.remote_name, "origin");
        assert!(
            config.github.link_stack,
            "linkStack defaults on with no [github] table"
        );
    }

    #[test]
    fn config_with_optional_fields() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path).expect("Failed to create Jujutsu instance");
        // Set all config including optional fields
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.host",
            "https://gitlab.example.com",
        ])
        .expect("Failed to set config");

        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.project",
            "my-group/my-project",
        ])
        .expect("Failed to set config");

        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.token",
            "glpat-test123",
        ])
        .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.branchPrefix", "mrs/"])
            .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.remoteName", "upstream"])
            .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.defaultBranch", "master"])
            .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.forge", "gitlab"])
            .expect("Failed to set config");

        // Load config
        let config = load_isolated(&repo_path).expect("Failed to load config");

        assert_eq!(config.gitlab.host, "https://gitlab.example.com".to_owned());
        assert_eq!(config.gitlab.project, "my-group/my-project".to_owned());
        assert_eq!(config.gitlab.token, "glpat-test123".to_owned());
        assert_eq!(config.remote_name, "upstream");
    }

    #[test]
    fn config_default_stack_visualization() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path).expect("Failed to create Jujutsu instance");
        // Set required config, but not stack visualization config
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.host",
            "https://gitlab.com",
        ])
        .expect("Failed to set config");

        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.project",
            "test/proj",
        ])
        .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.gitlab.token", "token"])
            .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.forge", "gitlab"])
            .expect("Failed to set config");

        let config = load_isolated(&repo_path).expect("Failed to load config");

        assert!(config.description.enabled);
        assert!(matches!(
            config.description.diagram.single,
            DescriptionDiagramFormat::None
        ));
        assert!(matches!(
            config.description.diagram.linear,
            DescriptionDiagramFormat::Linear
        ));
        assert!(matches!(
            config.description.diagram.tree,
            DescriptionDiagramFormat::Linear
        ));
        assert!(matches!(
            config.description.diagram.complex,
            DescriptionDiagramFormat::Linear
        ));
        assert_eq!(config.description.placement, StackPlacement::Bottom);
    }

    #[test]
    fn config_explicit_stack_visualization() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path).expect("Failed to create Jujutsu instance");
        // Set required config
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.host",
            "https://gitlab.com",
        ])
        .expect("Failed to set config");

        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.project",
            "test/proj",
        ])
        .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.gitlab.token", "token"])
            .expect("Failed to set config");

        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.description.enabled",
            "false",
        ])
        .expect("Failed to set config");
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.description.placement",
            "top",
        ])
        .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.forge", "gitlab"])
            .expect("Failed to set config");

        let config = load_isolated(&repo_path).expect("Failed to load config");

        assert!(!config.description.enabled);
        assert!(matches!(
            config.description.diagram.single,
            DescriptionDiagramFormat::None
        ));
        assert!(matches!(
            config.description.diagram.linear,
            DescriptionDiagramFormat::Linear
        ));
        assert!(matches!(
            config.description.diagram.tree,
            DescriptionDiagramFormat::Linear
        ));
        assert!(matches!(
            config.description.diagram.complex,
            DescriptionDiagramFormat::Linear
        ));
        assert_eq!(config.description.placement, StackPlacement::Top);
    }

    #[test]
    fn config_default_mr_settings() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path).expect("Failed to create Jujutsu instance");
        // Set required config only, don't set MR settings
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.host",
            "https://gitlab.com",
        ])
        .expect("Failed to set config");

        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.project",
            "test/proj",
        ])
        .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.gitlab.token", "token"])
            .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.forge", "gitlab"])
            .expect("Failed to set config");

        let config = load_isolated(&repo_path).expect("Failed to load config");

        assert!(config.delete_source_branch);
        assert!(!config.squash_commits);
    }

    #[test]
    fn config_explicit_mr_settings() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path).expect("Failed to create Jujutsu instance");
        // Set required config
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.host",
            "https://gitlab.com",
        ])
        .expect("Failed to set config");

        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.project",
            "test/proj",
        ])
        .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.gitlab.token", "token"])
            .expect("Failed to set config");

        // Set explicit MR settings (opposite of defaults)
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.deleteSourceBranch",
            "false",
        ])
        .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.squashCommits", "true"])
            .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.forge", "gitlab"])
            .expect("Failed to set config");

        let config = load_isolated(&repo_path).expect("Failed to load config");

        assert!(!config.delete_source_branch);
        assert!(config.squash_commits);
    }

    #[test]
    fn config_default_assign_to_self() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path).expect("Failed to create Jujutsu instance");
        // Set required config only
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.host",
            "https://gitlab.com",
        ])
        .expect("Failed to set config");

        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.project",
            "test/proj",
        ])
        .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.gitlab.token", "token"])
            .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.forge", "gitlab"])
            .expect("Failed to set config");

        let config = load_isolated(&repo_path).expect("Failed to load config");

        assert!(!config.assign_to_self);
    }

    #[test]
    fn config_explicit_assign_to_self() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path).expect("Failed to create Jujutsu instance");
        // Set required config
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.host",
            "https://gitlab.com",
        ])
        .expect("Failed to set config");

        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.project",
            "test/proj",
        ])
        .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.gitlab.token", "token"])
            .expect("Failed to set config");

        // Set assign_to_self to true
        jj.exec(["config", "set", "--repo", "jj-vine.assignToSelf", "true"])
            .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.forge", "gitlab"])
            .expect("Failed to set config");

        let config = load_isolated(&repo_path).expect("Failed to load config");

        assert!(config.assign_to_self);
    }

    #[test]
    fn config_default_reviewers_empty() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path).expect("Failed to create Jujutsu instance");
        // Set required config only
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.host",
            "https://gitlab.com",
        ])
        .expect("Failed to set config");

        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.project",
            "test/proj",
        ])
        .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.gitlab.token", "token"])
            .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.forge", "gitlab"])
            .expect("Failed to set config");

        let config = load_isolated(&repo_path).expect("Failed to load config");

        assert!(config.default_reviewers.is_empty());
    }

    #[test]
    fn config_default_reviewers_single() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path).expect("Failed to create Jujutsu instance");
        // Set required config
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.host",
            "https://gitlab.com",
        ])
        .expect("Failed to set config");

        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.project",
            "test/proj",
        ])
        .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.gitlab.token", "token"])
            .expect("Failed to set config");

        // Set single reviewer as TOML array
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.defaultReviewers",
            r#"["reviewer1"]"#,
        ])
        .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.forge", "gitlab"])
            .expect("Failed to set config");

        let config = load_isolated(&repo_path).expect("Failed to load config");

        assert_eq!(config.default_reviewers, vec!["reviewer1"]);
    }

    #[test]
    fn config_default_reviewers_multiple() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path).expect("Failed to create Jujutsu instance");
        // Set required config
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.host",
            "https://gitlab.com",
        ])
        .expect("Failed to set config");

        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.gitlab.project",
            "test/proj",
        ])
        .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.gitlab.token", "token"])
            .expect("Failed to set config");

        // Set multiple reviewers as TOML array
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.defaultReviewers",
            r#"["reviewer1", "reviewer2", "reviewer3"]"#,
        ])
        .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.forge", "gitlab"])
            .expect("Failed to set config");

        let config = load_isolated(&repo_path).expect("Failed to load config");

        assert_eq!(
            config.default_reviewers,
            vec!["reviewer1", "reviewer2", "reviewer3"]
        );
    }

    #[test]
    fn gitlab_direct_mode_without_target() {
        let config = GitLabConfig {
            host: "https://gitlab.com".to_owned(),
            project: "myuser/myrepo".to_owned(),
            target_project: String::new(),
            token: "token".to_owned(),
            create_merge_request_dependencies: true,
        };

        assert_eq!(config.target_project(), "myuser/myrepo");
        assert_eq!(config.source_project(), "myuser/myrepo");
        assert!(!config.is_fork_workflow());
    }

    #[test]
    fn gitlab_fork_mode_with_different_target() {
        let config = GitLabConfig {
            host: "https://gitlab.com".to_owned(),
            project: "myuser/fork".to_owned(),
            target_project: "upstream/repo".to_owned(),
            token: "token".to_owned(),
            create_merge_request_dependencies: true,
        };

        assert_eq!(config.target_project(), "upstream/repo");
        assert_eq!(config.source_project(), "myuser/fork");
        assert!(config.is_fork_workflow());
    }

    #[test]
    fn gitlab_fork_mode_with_same_target() {
        let config = GitLabConfig {
            host: "https://gitlab.com".to_owned(),
            project: "myuser/repo".to_owned(),
            target_project: "myuser/repo".to_owned(),
            token: "token".to_owned(),
            create_merge_request_dependencies: true,
        };

        assert_eq!(config.target_project(), "myuser/repo");
        assert_eq!(config.source_project(), "myuser/repo");
        assert!(!config.is_fork_workflow());
    }

    #[test]
    fn gitlab_project_rejects_clone_url() {
        let config = Config::builder()
            .forge(ForgeType::GitLab)
            .gitlab(GitLabConfig {
                host: "https://gitlab.com".to_owned(),
                project: "git@gitlab.com:myuser/repo.git".to_owned(),
                target_project: String::new(),
                token: "token".to_owned(),
                create_merge_request_dependencies: true,
            })
            .build();

        let err = config.validate().unwrap_err().to_string();

        assert!(err.contains("gitlab.project must be a GitLab project path or numeric ID"));
    }

    #[test]
    fn gitlab_target_project_rejects_clone_url() {
        let config = Config::builder()
            .forge(ForgeType::GitLab)
            .gitlab(GitLabConfig {
                host: "https://gitlab.com".to_owned(),
                project: "myuser/fork".to_owned(),
                target_project: "https://gitlab.com/upstream/repo.git".to_owned(),
                token: "token".to_owned(),
                create_merge_request_dependencies: true,
            })
            .build();

        let err = config.validate().unwrap_err().to_string();

        assert!(err.contains("gitlab.targetProject must be a GitLab project path or numeric ID"));
    }

    #[test]
    fn github_direct_mode_without_target() {
        let config = GitHubConfig {
            host: "https://api.github.com".to_owned(),
            project: "myuser/myrepo".to_owned(),
            target_project: String::new(),
            token: "token".to_owned(),
            token_command: Vec::new(),
            link_stack: true,
        };

        assert_eq!(config.target_project(), "myuser/myrepo");
        assert_eq!(config.source_project(), "myuser/myrepo");
        assert!(!config.is_fork_workflow());
    }

    #[test]
    fn github_link_stack_defaults_to_true_when_absent() {
        let config: GitHubConfig =
            toml::from_str("project = \"owner/repo\"").expect("parse GitHubConfig");
        assert!(config.link_stack);
    }

    #[test]
    fn github_link_stack_defaults_to_true_without_github_table() {
        assert!(GitHubConfig::default().link_stack);
    }

    #[test]
    fn github_link_stack_false_parses_to_false() {
        let config: GitHubConfig = toml::from_str("linkStack = false").expect("parse GitHubConfig");
        assert!(!config.link_stack);
    }

    #[test]
    fn github_link_stack_true_parses_to_true() {
        let config: GitHubConfig = toml::from_str("linkStack = true").expect("parse GitHubConfig");
        assert!(config.link_stack);
    }

    #[test]
    fn resolved_token_errors_when_neither_source_is_configured() {
        let error = GitHubConfig::default()
            .resolved_token()
            .expect_err("missing token sources must error");

        assert!(
            error
                .to_string()
                .contains("github.token or github.tokenCommand")
        );
    }

    #[test]
    fn resolved_token_trims_literal_token() {
        let config = GitHubConfig {
            token: "literal-token\n".to_owned(),
            ..GitHubConfig::default()
        };

        assert_eq!(
            config.resolved_token().expect("literal token"),
            "literal-token"
        );
    }

    #[test]
    fn resolved_token_prefers_literal_over_command() {
        let config = GitHubConfig {
            token: " literal-token\n".to_owned(),
            token_command: vec!["false".to_owned()],
            ..GitHubConfig::default()
        };
        assert_eq!(
            config.resolved_token().expect("literal token"),
            "literal-token"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolved_token_runs_command_and_trims() {
        let config = GitHubConfig {
            token_command: vec!["printf".to_owned(), "  command-token\n".to_owned()],
            ..GitHubConfig::default()
        };
        assert_eq!(
            config.resolved_token().expect("command token"),
            "command-token"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolved_token_rejects_nonzero_exit_without_stderr() {
        let config = GitHubConfig {
            token_command: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "printf '%s_%s' \"$1\" \"$2\" >&2; exit 1".to_owned(),
                "sh".to_owned(),
                "SAFE_ARG_A".to_owned(),
                "SAFE_ARG_B".to_owned(),
            ],
            ..GitHubConfig::default()
        };
        let message = config
            .resolved_token()
            .expect_err("non-zero exit")
            .to_string();
        assert!(message.contains("failed"));
        assert!(!message.contains("SAFE_ARG_A_SAFE_ARG_B"));
        assert!(!message.contains("SAFE_ARG_A"));
        assert!(!message.contains("SAFE_ARG_B"));
    }

    #[cfg(unix)]
    #[test]
    fn resolved_token_rejects_non_utf8_output() {
        let config = GitHubConfig {
            token_command: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "printf '\\377\\376'".to_owned(),
            ],
            ..GitHubConfig::default()
        };
        let message = config
            .resolved_token()
            .expect_err("non-UTF-8 output")
            .to_string();
        assert!(message.contains("non-UTF-8"));
    }

    #[cfg(unix)]
    #[test]
    fn resolved_token_rejects_empty_output() {
        let config = GitHubConfig {
            token_command: vec!["true".to_owned()],
            ..GitHubConfig::default()
        };
        let message = config
            .resolved_token()
            .expect_err("empty output")
            .to_string();
        assert!(message.contains("empty output"));
    }

    #[test]
    fn resolved_token_rejects_missing_binary() {
        let config = GitHubConfig {
            token_command: vec!["jj-vine-no-such-token-bin".to_owned()],
            ..GitHubConfig::default()
        };
        let message = config
            .resolved_token()
            .expect_err("missing binary")
            .to_string();
        assert!(message.contains("not found in PATH"));
    }

    #[cfg(unix)]
    #[test]
    fn resolved_token_timeout_kills_helper() {
        let config = GitHubConfig {
            token_command: vec!["sh".to_owned(), "-c".to_owned(), "sleep 30".to_owned()],
            ..GitHubConfig::default()
        };
        let start = std::time::Instant::now();
        let message = config
            .resolved_token_with_timeout(core::time::Duration::from_millis(200))
            .expect_err("helper must time out")
            .to_string();
        assert!(message.contains("timed out"));
        assert!(start.elapsed() < core::time::Duration::from_secs(5));
    }

    /// A token command whose run would start a marker file, so a test can
    /// tell whether it was spawned.
    #[cfg(unix)]
    fn marker_token_command(marker: &Path) -> GitHubConfig {
        GitHubConfig {
            token_command: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "touch \"$1\"; printf command-token".to_owned(),
                "_".to_owned(),
                marker.display().to_string(),
            ],
            ..GitHubConfig::default()
        }
    }

    /// A `PATH` lookup that finds `sh` only after `delay`, standing in for a
    /// slow filesystem.
    #[cfg(unix)]
    fn slow_lookup(delay: core::time::Duration) -> impl FnOnce(&str) -> Option<PathBuf> {
        move |bin| {
            std::thread::sleep(delay);
            which::which(bin).ok()
        }
    }

    #[cfg(unix)]
    #[test]
    fn token_lookup_past_phase_deadline_never_spawns_helper() {
        let temp = TempDir::new().expect("tempdir");
        let marker = temp.path().join("ran");
        let config = marker_token_command(&marker);
        let phase_deadline = std::time::Instant::now() + core::time::Duration::from_millis(50);

        let message = config
            .resolve_token(
                TOKEN_COMMAND_TIMEOUT,
                Some(phase_deadline),
                slow_lookup(core::time::Duration::from_millis(200)),
            )
            .expect_err("a lookup that ends past the phase deadline must time out")
            .to_string();

        assert!(message.contains("timed out"), "got {message}");
        assert!(
            !marker.exists(),
            "the helper must not spawn after the deadline"
        );
    }

    #[cfg(unix)]
    #[test]
    fn token_timeout_counts_lookup_time() {
        let temp = TempDir::new().expect("tempdir");
        let marker = temp.path().join("ran");
        let config = marker_token_command(&marker);

        let message = config
            .resolve_token(
                core::time::Duration::from_millis(50),
                None,
                slow_lookup(core::time::Duration::from_millis(200)),
            )
            .expect_err("a lookup that outlasts the token timeout must time out")
            .to_string();

        assert!(message.contains("timed out"), "got {message}");
        assert!(
            !marker.exists(),
            "the helper must not spawn after its timeout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolved_token_whitespace_literal_uses_command() {
        let config = GitHubConfig {
            token: " \n".to_owned(),
            token_command: vec!["printf".to_owned(), "command-token".to_owned()],
            ..GitHubConfig::default()
        };
        assert_eq!(
            config.resolved_token().expect("command token"),
            "command-token"
        );
    }

    /// A `tokenCommand` argv that prints `token` to stdout and nothing else,
    /// with a shell present on each platform.
    fn printing_token_command(token: &str) -> Vec<String> {
        if cfg!(windows) {
            vec![
                "powershell".to_owned(),
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-Command".to_owned(),
                format!("[Console]::Out.Write('{token}')"),
            ]
        } else {
            vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "printf '%s' \"$1\"".to_owned(),
                "_".to_owned(),
                token.to_owned(),
            ]
        }
    }

    /// The `tokenCommand` key in a real jj config file reaches
    /// `resolved_token` through `Config::load`, with no literal token set.
    #[test]
    fn config_load_token_command_resolves_token() {
        const TOKEN: &str = "fixture-command-token";

        let (temp, repo_path) = create_test_repo();
        let mut github = toml::Table::new();
        github.insert("project".to_owned(), "owner/repo".into());
        github.insert(
            "tokenCommand".to_owned(),
            printing_token_command(TOKEN).into(),
        );
        let mut jj_vine = toml::Table::new();
        jj_vine.insert("forge".to_owned(), "github".into());
        jj_vine.insert("github".to_owned(), github.into());
        let mut root = toml::Table::new();
        root.insert("jj-vine".to_owned(), jj_vine.into());
        // Overwrite the isolated user config, so the key is read from TOML as
        // a user writes it, not built in code.
        std::fs::write(
            temp.path().join(ISOLATED_TEST_CONFIG),
            toml::to_string(&root).expect("the fixture table must serialize"),
        )
        .expect("Failed to write isolated config");

        let config = load_isolated(&repo_path).expect("Failed to load config");

        assert!(
            config.github.token.is_empty(),
            "no literal token is configured, so the command must supply it"
        );
        assert_eq!(config.github.token_command, printing_token_command(TOKEN));
        assert_eq!(
            config.github.resolved_token().expect("command token"),
            TOKEN
        );
    }

    #[test]
    fn push_config_resolves_default_custom_and_disabled_argv() {
        assert_eq!(
            RepoPushConfig::default().to_argv(),
            Some(vec!["jj".to_owned(), "git".to_owned(), "push".to_owned()])
        );
        assert_eq!(
            RepoPushConfig::Command(vec!["custom-push".to_owned(), "push".to_owned()]).to_argv(),
            Some(vec!["custom-push".to_owned(), "push".to_owned()])
        );
        assert_eq!(RepoPushConfig::Enabled(false).to_argv(), None);
    }

    #[test]
    fn push_config_no_hooks_uses_builtin_but_preserves_disabled() {
        assert_eq!(
            RepoPushConfig::Command(vec!["custom-push".to_owned(), "push".to_owned()])
                .resolve_argv(true),
            Some(vec!["jj".to_owned(), "git".to_owned(), "push".to_owned()])
        );
        assert_eq!(RepoPushConfig::Enabled(false).resolve_argv(true), None);
    }

    #[test]
    fn push_config_parses_custom_argv_from_jj_config() -> Result<()> {
        let (_temp, repo_path) = create_test_repo();
        let jj = isolated_jj(&repo_path)?;
        jj.exec(["config", "set", "--repo", "jj-vine.forge", "forgejo"])?;
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.forgejo.host",
            "https://forgejo.example",
        ])?;
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.forgejo.project",
            "owner/repository",
        ])?;
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.forgejo.token",
            "test-token",
        ])?;
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.push",
            r#"["custom-push", "push"]"#,
        ])?;

        let config = load_isolated(&repo_path)?;

        assert_eq!(
            config.push,
            RepoPushConfig::Command(vec!["custom-push".to_owned(), "push".to_owned()])
        );
        Ok(())
    }

    #[test]
    fn push_config_rejects_empty_command() -> Result<()> {
        let (_temp, repo_path) = create_test_repo();
        let jj = isolated_jj(&repo_path)?;
        jj.exec(["config", "set", "--repo", "jj-vine.forge", "forgejo"])?;
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.forgejo.host",
            "https://forgejo.example",
        ])?;
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.forgejo.project",
            "owner/repository",
        ])?;
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.forgejo.token",
            "test-token",
        ])?;
        jj.exec(["config", "set", "--repo", "jj-vine.push", "[]"])?;

        let message = load_isolated(&repo_path)
            .expect_err("an empty push command is rejected")
            .to_string();

        assert!(message.contains("jj-vine.push"), "{message}");
        Ok(())
    }
}
