#![expect(clippy::module_name_repetitions, reason = "fine for Config")]

use std::path::PathBuf;

use bon::Builder;
use serde::{Deserialize, de::Visitor};

use crate::{
    error::{ConfigSnafu, Error, Result},
    jj::Jujutsu,
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

/// Where the jj-vine stack block is placed in a PR/MR description.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum StackPlacement {
    /// Stack block after the user's description (current default).
    #[default]
    Bottom,

    /// Stack block before the user's description.
    Top,
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

    /// The branch name to use for MRs into `trunk()`. You will generally only
    /// need to set this explicitly if you use a branch name other than
    /// `main`, `master`, or `trunk`, and jj-vine is having difficulty
    /// detecting the correct branch name automatically.
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

    /// The command jj-vine runs to push bookmarks to the remote, or `true`
    /// for the built-in default. Defaults to `true`, equivalent to
    /// `["jj", "git", "push"]` (jj-vine's original hardcoded push). Set to an
    /// array to route the push through a different command — for example
    /// `["jj-hp", "push"]` runs the push through the jj-hooks pre-push gate so
    /// a bare `jj-vine submit` cannot bypass it. The remote (`--remote <name>`)
    /// and each bookmark (`--bookmark <name>`) are appended to whatever command
    /// this resolves to, so the command must accept `jj git push`'s flags
    /// (`jj-hp push` does). Unlike `fetch`, the array is a full argv whose
    /// first element is the binary to run (not implicitly `jj`), since the
    /// point is to reach a *different* binary. `jj-vine submit --no-hooks`
    /// overrides this for one run, forcing the built-in `jj git push`.
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

    /// GitHub configuration.
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
    /// If true, the built-in `jj git push`. If false, pushing is disabled
    /// (jj-vine plans and updates MRs but never pushes — rarely useful, but
    /// symmetric with `fetch` and lets a caller stage a push separately).
    Enabled(bool),

    /// Runs this command to push, as a full argv whose first element is the
    /// binary. `["jj-hp", "push"]` routes the push through the jj-hooks gate;
    /// `["jj", "git", "push"]` is the explicit form of the default.
    Command(Vec<String>),
}

impl RepoPushConfig {
    /// The push command as an owned argv, or `None` when pushing is disabled.
    /// The default (`Enabled(true)`) and the explicit `["jj","git","push"]`
    /// both yield jj-vine's original push; a `Command` yields its own argv.
    /// `--remote`/`--bookmark` args are appended by the caller.
    #[must_use]
    pub fn to_argv(&self) -> Option<Vec<String>> {
        match self {
            RepoPushConfig::Enabled(true) => Some(Self::builtin_push_argv()),
            RepoPushConfig::Command(command) => Some(command.clone()),
            RepoPushConfig::Enabled(false) => None,
        }
    }

    /// jj-vine's built-in push argv (`jj git push`) — the default form and the
    /// form `--no-hooks` forces. One source of truth so the default and the
    /// no-hooks escape can never silently diverge.
    fn builtin_push_argv() -> Vec<String> {
        ["jj", "git", "push"]
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    }

    /// Resolve the push argv for one run, honoring `--no-hooks`.
    ///
    /// `--no-hooks` bypasses a *configured* gate command (e.g. `["jj-hp",
    /// "push"]`) by forcing the built-in `jj git push` for this run. It does
    /// NOT re-enable a push that config disabled: `push = false` stays a no-op
    /// (`None`) regardless of `--no-hooks`. `None` means pushing is disabled.
    #[must_use]
    pub fn resolve_argv(&self, no_hooks: bool) -> Option<Vec<String>> {
        match self.to_argv() {
            Some(_) if no_hooks => Some(Self::builtin_push_argv()),
            other => other,
        }
    }
}

impl Default for RepoPushConfig {
    fn default() -> Self {
        Self::Enabled(true)
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

#[derive(Debug, Clone, Deserialize, Default)]
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
    /// first element is the binary (e.g. `["gh", "auth", "token"]`). Used only
    /// when `token` is empty; the trimmed stdout of the command becomes the
    /// token. This keeps the secret out of committed/global config: the token
    /// is resolved fresh per run from a credential helper instead of stored.
    /// A non-zero exit or empty output is an error.
    #[serde(default)]
    pub token_command: Vec<String>,

    /// Register submitted PR stacks as GitHub-native stacks via `gh-stack link`
    /// after a successful submit (defaults to true). GitHub-only; requires the
    /// `gh-stack` binary on PATH. Non-fatal: a link failure warns, never fails
    /// the submit.
    #[serde(default = "default_true")]
    pub link_stack: bool,
}

/// Maximum wall-clock time to wait for a `tokenCommand` credential helper.
/// The fleet default (`gh auth token`) returns in milliseconds; a helper that
/// blocks on a keychain unlock, a network credential store, or an interactive
/// prompt would otherwise hang `submit`/`status` forever, so it is bounded.
const TOKEN_COMMAND_TIMEOUT: core::time::Duration = core::time::Duration::from_secs(10);

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

    /// The GitHub token, resolved from config. A non-empty literal `token`
    /// wins (trimmed); otherwise `token_command` is run and its trimmed stdout
    /// is the token. Returns an error if neither is set, if the command binary
    /// is not on `PATH`, if the command exits non-zero, if its output is empty
    /// or not valid UTF-8, or if the command overruns `TOKEN_COMMAND_TIMEOUT`.
    /// An error never includes the command's raw stderr (which can carry the
    /// credential).
    pub fn resolved_token(&self) -> Result<String> {
        self.resolved_token_with_timeout(TOKEN_COMMAND_TIMEOUT)
    }

    /// `resolved_token` with an injectable helper timeout, so the timeout arm
    /// is unit-testable without a real multi-second sleep.
    fn resolved_token_with_timeout(&self, timeout: core::time::Duration) -> Result<String> {
        // Trim the literal too (symmetry with the command path), so a literal
        // with stray whitespace does not reach the Authorization header as an
        // opaque 401, and an all-whitespace literal falls through to the
        // command rather than returning blank.
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

        let bin_path = which::which(bin).map_err(|e| {
            ConfigSnafu {
                message: format!("github.tokenCommand binary `{bin}` not found in PATH: {e}"),
            }
            .build()
        })?;

        let mut command = std::process::Command::new(&bin_path);
        command.args(args);
        let Some(output) = crate::process::output_with_timeout(command, timeout)? else {
            return Err(ConfigSnafu {
                message: format!(
                    "github.tokenCommand `{}` timed out after {}s",
                    self.token_command.join(" "),
                    timeout.as_secs()
                ),
            }
            .build());
        };
        if !output.status.success() {
            // Report only the command and exit status - never the helper's raw
            // stderr, which can contain the credential itself or sensitive
            // diagnostics that would then leak into terminal/CI logs.
            return Err(ConfigSnafu {
                message: format!(
                    "github.tokenCommand `{}` failed with {}",
                    self.token_command.join(" "),
                    output.status
                ),
            }
            .build());
        }

        // Reject invalid UTF-8 at this boundary rather than lossily replacing
        // bytes with U+FFFD, which would silently corrupt the token and surface
        // later as an opaque GitHub 401.
        let raw = String::from_utf8(output.stdout).map_err(|_| {
            ConfigSnafu {
                message: format!(
                    "github.tokenCommand `{}` produced non-UTF-8 output",
                    self.token_command.join(" ")
                ),
            }
            .build()
        })?;
        let token = raw.trim().to_owned();
        if token.is_empty() {
            return Err(ConfigSnafu {
                message: format!(
                    "github.tokenCommand `{}` produced empty output",
                    self.token_command.join(" ")
                ),
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

    /// Where to place the stack block relative to the user's description.
    /// Defaults to `bottom` (after the user content).
    #[serde(default)]
    pub placement: StackPlacement,
}

impl Default for DescriptionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sync: false,
            diagram: DescriptionDiagramConfig::default(),
            single_revision: DescriptionMode::NotFirstLine,
            multiple_revisions: DescriptionMode::CommitListFull,
            placement: StackPlacement::Bottom,
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

/// Read the repo-layer (`.jj/repo/config.toml`) values of `jj-vine.github`
/// via `jj config list --repo`, which emits ONLY the repo layer (verified at
/// implementation time — global/user keys do not appear). Best-effort: any
/// subprocess or parse failure yields an empty map, collapsing every
/// repo-layer check to "absent" so derivation proceeds. Used by
/// [`Config::load_with`] to apply the (b)-precedence rule: a repo-layer
/// non-empty value wins over clone derivation.
fn repo_layer_values(jj: &Jujutsu) -> toml::Table {
    let Ok(output) = jj.exec(["config", "list", "--repo"]) else {
        return toml::Table::new();
    };
    toml::from_str::<toml::Table>(&output.stdout)
        .ok()
        .and_then(|table| table.get("jj-vine").cloned())
        .and_then(|jj_vine| jj_vine.get("github").cloned())
        .and_then(|github| github.as_table().cloned())
        .unwrap_or_default()
}

/// Whether the repo layer set `jj-vine.github.<key>` to a non-empty string.
/// A repo-local empty value (`project = ""`) is treated as ABSENT — it
/// collapses into the derive path (design ruling (b), the explicit-empty
/// test), rather than being honored as an explicit empty.
fn repo_layer_nonempty(github: &toml::Table, key: &str) -> bool {
    github
        .get(key)
        .and_then(toml::Value::as_str)
        .is_some_and(|value| !value.is_empty())
}

impl Config {
    /// Load configuration from jj config.
    pub fn load(repo_path: impl Into<PathBuf>) -> Result<Self> {
        Self::load_with(&Jujutsu::new(repo_path)?)
    }

    /// Load configuration through an already-built [`Jujutsu`]. Split out of
    /// [`Config::load`] so tests can supply a config-isolated instance
    /// (`Jujutsu::new_isolated`) without changing the public entry point.
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

        // Derive github.project/github.host from the clone's own remote when
        // the repo config layer left them unset. Precedence (design ruling
        // (b)): repo-explicit > clone-derived > global-explicit — so a value
        // the *repo* layer set non-empty is honored, but a value only the
        // global layer set is superseded by the clone-derived one. GitHub-only;
        // best-effort (any failure falls through to static config, then
        // validate()). A repo-local empty value is treated as absent → derives.
        if config.forge == ForgeType::GitHub {
            let repo_layer = repo_layer_values(jj);
            let project_set = repo_layer_nonempty(&repo_layer, "project");
            let host_set = repo_layer_nonempty(&repo_layer, "host");
            if (!project_set || !host_set)
                && let Some(detected) =
                    crate::remote::detect_project(jj, &config.remote_name, ForgeType::GitHub)
            {
                if !project_set {
                    config.github.project = detected.project;
                }
                if !host_set {
                    config.github.host = detected.host;
                }
            }
        }

        config.validate()?;

        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if let RepoPushConfig::Command(command) = &self.push
            && command.is_empty()
        {
            return Err(ConfigSnafu {
                message:
                    "jj-vine.push must not be an empty command; use `false` to disable pushing"
                        .to_owned(),
            }
            .build());
        }

        match self.forge {
            ForgeType::GitLab => crate::forge::gitlab::validate_config(self),
            ForgeType::GitHub => crate::forge::github::validate_config(self),
            ForgeType::Forgejo => crate::forge::forgejo::validate_config(self),
            ForgeType::AzureDevOps => crate::forge::azure::validate_config(self),
        }?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;

    /// Name of the stand-in user config every test repo runs against. It lives
    /// beside the repo (not inside it, so it is never snapshotted) and is
    /// empty, so no user-level config *file* key reaches a test's resolved
    /// config. jj's env-derived keys (`JJ_USER`, `ui.editor` from `$EDITOR`,
    /// and so on) still apply, none of which live under `jj-vine`.
    const ISOLATED_CONFIG: &str = "isolated-user-config.toml";

    /// A [`Jujutsu`] for a `create_test_repo` repo with the user-level jj
    /// config swapped out for the empty isolated one. Every test *in this
    /// module* goes through this rather than [`Jujutsu::new`], or a stray key
    /// in the user's config leaks into the assertions. `jj.rs`'s own tests and
    /// the e2e harness in `tests::test_helpers` still construct unisolated
    /// instances, so the invariant is module-local, not crate-wide; closing
    /// that gap is SEA-1425.
    fn isolated_jj(repo_path: &Path) -> Jujutsu {
        let config_path = repo_path
            .parent()
            .expect("test repo always has a parent temp dir")
            .join(ISOLATED_CONFIG);
        Jujutsu::new_isolated(repo_path, config_path).expect("Failed to create Jujutsu instance")
    }

    /// [`Config::load`] against a config-isolated repo — the test-side
    /// equivalent of the public entry point.
    fn load_isolated(repo_path: &Path) -> Result<Config> {
        Config::load_with(&isolated_jj(repo_path))
    }

    fn create_test_repo() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().expect("Failed to create temp dir");

        // The repo is a child of the temp dir so the isolated user config can
        // sit alongside it, outside the working copy.
        std::fs::write(temp_dir.path().join(ISOLATED_CONFIG), "")
            .expect("Failed to write isolated config");
        let repo_path = temp_dir.path().join("repo");
        std::fs::create_dir_all(&repo_path).expect("Failed to create repo dir");

        let jj = isolated_jj(&repo_path);
        jj.exec(["git", "init", "--colocate"])
            .expect("Failed to init jj repo");

        (temp_dir, repo_path)
    }

    /// Guards the config isolation the whole test module rests on: `JJ_CONFIG`
    /// must reach the spawned `jj` process, and must *replace* the user-level
    /// layer rather than being merged in as an extra layer beside it.
    ///
    /// Both halves matter, and they fail independently. If a spawn path ever
    /// stops applying the override, the seeded key goes missing and the first
    /// assertion fires. If the override is ever downgraded to something
    /// additive — `--config-file`, say, which layers on top of the user config
    /// instead of standing in for it — the resolved user-layer path is still
    /// the developer's own, and the second assertion fires. Without this test
    /// either regression is silent: every other test in the module would keep
    /// passing on a machine whose user config happens to be empty, and start
    /// failing mysteriously on one whose is not.
    ///
    /// Hermetic by construction: it asserts the resolved user-layer path is
    /// exactly the file this test handed to `JJ_CONFIG`, so it never reads or
    /// mutates the process environment, and its verdict does not depend on
    /// where the developer's own config lives or what it contains.
    #[test]
    fn config_isolation_replaces_user_layer() {
        /// A key under the `jj-vine` table with a value no real user config
        /// would hold, so seeing this exact pair in `jj config list` can only
        /// have come from the file seeded below. It need not be a field the
        /// fork deserializes: the assertion matches `jj config list` text, and
        /// jj lists every key under the table.
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

        // Resolving the *path* of the user layer, rather than looking for the
        // absence of the seeded key, is what makes this discriminating: a
        // sentinel is unique to its own file, so its absence elsewhere proves
        // nothing. Under a true replacement the user layer IS the override
        // file; under any additive scheme it remains the developer's own.
        let resolved = seeded
            .exec(["config", "path", "--user"])
            .expect("Failed to resolve user config path")
            .stdout;
        assert_eq!(
            Path::new(resolved.trim()),
            seeded_config,
            "JJ_CONFIG must replace the user-level layer rather than stack \
             beside it, so the resolved user config is the override itself"
        );
    }

    #[test]
    fn config_load_missing_required() {
        let (_temp, repo_path) = create_test_repo();

        // Isolation makes this message deterministic, so assert it exactly. The
        // former substring OR accepted "jj-vine", which is also a substring of
        // the *leaked* message ("Failed to parse jj-vine config: missing field
        // `forge`") a stray user-level key produces — so it stayed green under
        // precisely the regression this module's isolation exists to prevent.
        // That leak is the fleet's default state, not a hypothetical: the
        // nix-managed global config sets `jj-vine.description.placement`
        // (personal/matt/nix/dotfiles/jj/config.toml).
        let result = load_isolated(&repo_path);
        let Err(Error::Config { message, .. }) = result else {
            panic!("Expected Config error for missing required field, got: {result:?}");
        };
        assert_eq!(message, "Missing required config section: jj-vine");
    }

    #[test]
    fn config_load_complete() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path);
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
    }

    #[test]
    fn config_with_optional_fields() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path);
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

        let jj = isolated_jj(&repo_path);
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
        assert!(matches!(
            config.description.placement,
            StackPlacement::Bottom
        ));
    }

    #[test]
    fn config_explicit_stack_placement() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path);
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
            "jj-vine.description.placement",
            "top",
        ])
        .expect("Failed to set config");

        jj.exec(["config", "set", "--repo", "jj-vine.forge", "gitlab"])
            .expect("Failed to set config");

        let config = load_isolated(&repo_path).expect("Failed to load config");

        assert!(matches!(config.description.placement, StackPlacement::Top));
    }

    #[test]
    fn config_explicit_stack_visualization() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path);
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
    }

    #[test]
    fn config_default_mr_settings() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path);
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

        let jj = isolated_jj(&repo_path);
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

        let jj = isolated_jj(&repo_path);
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

        let jj = isolated_jj(&repo_path);
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

        let jj = isolated_jj(&repo_path);
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

        let jj = isolated_jj(&repo_path);
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

        let jj = isolated_jj(&repo_path);
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
    fn push_rejects_empty_command() {
        let config = Config::builder()
            .forge(ForgeType::GitLab)
            .gitlab(GitLabConfig {
                host: "https://gitlab.com".to_owned(),
                project: "myuser/repo".to_owned(),
                target_project: String::new(),
                token: "token".to_owned(),
                create_merge_request_dependencies: true,
            })
            .push(RepoPushConfig::Command(vec![]))
            .build();

        let err = config.validate().unwrap_err().to_string();

        assert!(err.contains("jj-vine.push must not be an empty command"));
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
    fn github_link_stack_false_parses_to_false() {
        let config: GitHubConfig = toml::from_str("linkStack = false").expect("parse GitHubConfig");
        assert!(!config.link_stack);
    }

    #[test]
    fn github_link_stack_true_parses_to_true() {
        let config: GitHubConfig = toml::from_str("linkStack = true").expect("parse GitHubConfig");
        assert!(config.link_stack);
    }

    /// Set the minimal required `[jj-vine]` config (GitLab forge) so
    /// `Config::load` succeeds, mirroring `config_load_complete`. Callers layer
    /// the `push` key on top to exercise `RepoPushConfig` deserialization.
    fn set_required_gitlab(jj: &Jujutsu) {
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
    }

    #[test]
    fn repo_push_config_to_argv_enabled_is_jj_git_push() {
        // The enabled variant resolves to jj-vine's original hardcoded push;
        // this exact argv is what `push_bookmarks` ultimately executes.
        assert_eq!(
            RepoPushConfig::Enabled(true).to_argv(),
            Some(vec!["jj".to_owned(), "git".to_owned(), "push".to_owned()]),
            "Enabled(true) must resolve to `jj git push`"
        );
    }

    #[test]
    fn repo_push_config_to_argv_command_is_verbatim() {
        // A custom command is returned exactly, so a bare submit reaches the
        // configured binary (e.g. the jj-hp pre-push gate) instead of jj.
        assert_eq!(
            RepoPushConfig::Command(vec!["jj-hp".to_owned(), "push".to_owned()]).to_argv(),
            Some(vec!["jj-hp".to_owned(), "push".to_owned()]),
            "Command variant must return its argv verbatim"
        );
    }

    #[test]
    fn repo_push_config_to_argv_disabled_is_none() {
        // `push = false` disables pushing: there is no command to run.
        assert_eq!(
            RepoPushConfig::Enabled(false).to_argv(),
            None,
            "Enabled(false) must disable pushing"
        );
    }

    #[test]
    fn resolve_argv_no_hooks_preserves_disabled_push() {
        // OQ4-B: `--no-hooks` skips a configured gate but must NOT re-enable a
        // push disabled by config (`push = false`); it stays a no-op.
        assert_eq!(RepoPushConfig::Enabled(false).resolve_argv(true), None);
    }

    #[test]
    fn resolve_argv_no_hooks_forces_builtin_over_gate() {
        // With a configured gate command, `--no-hooks` bypasses it to the
        // built-in `jj git push` for this run.
        assert_eq!(
            RepoPushConfig::Command(vec!["jj-hp".to_owned(), "push".to_owned()]).resolve_argv(true),
            Some(vec!["jj".to_owned(), "git".to_owned(), "push".to_owned()]),
        );
    }

    #[test]
    fn resolve_argv_hooks_honors_configured_command() {
        // Without `--no-hooks`, the configured gate command is used verbatim,
        // and a disabled push stays disabled.
        assert_eq!(
            RepoPushConfig::Command(vec!["jj-hp".to_owned(), "push".to_owned()])
                .resolve_argv(false),
            Some(vec!["jj-hp".to_owned(), "push".to_owned()]),
        );
        assert_eq!(RepoPushConfig::Enabled(false).resolve_argv(false), None);
    }

    #[test]
    fn config_push_defaults_to_enabled_when_absent() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path);
        set_required_gitlab(&jj);

        let config = load_isolated(&repo_path).expect("Failed to load config");

        // No `jj-vine.push` key: the field is optional and resolves to the
        // push-enabled variant, so a config that never mentions push still
        // pushes with the built-in command.
        assert_eq!(
            config.push,
            RepoPushConfig::Enabled(true),
            "an absent push key must deserialize to Enabled(true)"
        );
    }

    #[test]
    fn config_push_true_parses_as_enabled() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path);
        set_required_gitlab(&jj);
        jj.exec(["config", "set", "--repo", "jj-vine.push", "true"])
            .expect("Failed to set config");

        let config = load_isolated(&repo_path).expect("Failed to load config");

        assert_eq!(
            config.push,
            RepoPushConfig::Enabled(true),
            "push = true must deserialize to Enabled(true)"
        );
    }

    #[test]
    fn config_push_false_parses_as_disabled() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path);
        set_required_gitlab(&jj);
        jj.exec(["config", "set", "--repo", "jj-vine.push", "false"])
            .expect("Failed to set config");

        let config = load_isolated(&repo_path).expect("Failed to load config");

        assert_eq!(
            config.push,
            RepoPushConfig::Enabled(false),
            "push = false must deserialize to Enabled(false)"
        );
    }

    #[test]
    fn config_push_array_parses_as_command() {
        let (_temp, repo_path) = create_test_repo();

        let jj = isolated_jj(&repo_path);
        set_required_gitlab(&jj);
        jj.exec([
            "config",
            "set",
            "--repo",
            "jj-vine.push",
            r#"["jj-hp", "push"]"#,
        ])
        .expect("Failed to set config");

        let config = load_isolated(&repo_path).expect("Failed to load config");

        // An array is a full argv (untagged: bool -> Enabled, array -> Command),
        // routing the push through a different binary (the jj-hp gate).
        assert_eq!(
            config.push,
            RepoPushConfig::Command(vec!["jj-hp".to_owned(), "push".to_owned()]),
            "push = [\"jj-hp\", \"push\"] must deserialize to the Command variant"
        );
    }

    #[test]
    fn resolved_token_prefers_literal_over_command() {
        // A non-empty literal token wins even when a command is also set, so a
        // per-repo literal override always beats the global command.
        let cfg = GitHubConfig {
            token: "literal-tok".to_owned(),
            token_command: vec!["false".to_owned()],
            ..GitHubConfig::default()
        };
        assert_eq!(
            cfg.resolved_token().expect("literal token must resolve"),
            "literal-tok",
            "a non-empty literal token must win over tokenCommand"
        );
    }

    #[test]
    fn resolved_token_runs_command_and_trims() {
        // With no literal token, the command runs and its trimmed stdout is the
        // token — this is the fleet default (e.g. `gh auth token`).
        let cfg = GitHubConfig {
            token_command: vec!["printf".to_owned(), "  cmd-tok\n".to_owned()],
            ..GitHubConfig::default()
        };
        assert_eq!(
            cfg.resolved_token().expect("command token must resolve"),
            "cmd-tok",
            "tokenCommand stdout must be trimmed and used as the token"
        );
    }

    #[test]
    fn resolved_token_errors_when_neither_set() {
        // Neither a literal nor a command: a clear config error, not a silent
        // empty token that would later fail as a 401.
        let err = GitHubConfig::default()
            .resolved_token()
            .expect_err("no token or tokenCommand must error");
        assert!(
            err.to_string()
                .contains("github.token or github.tokenCommand"),
            "error must name both config keys, got: {err}"
        );
    }

    #[test]
    fn resolved_token_errors_on_nonzero_exit() {
        // A command that exits non-zero is an error (not an empty/garbage
        // token). The error reports the command and exit status but MUST NOT
        // include the helper's raw stderr, which can carry the credential.
        // The helper concatenates two argv fragments to stderr, so the full
        // sentinel `SECRET-LEAK` exists only in the child's stderr, never in
        // the argv the error message legitimately echoes.
        let cfg = GitHubConfig {
            token_command: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "printf '%s%s' \"$1\" \"$2\" >&2; exit 1".to_owned(),
                "sh".to_owned(),
                "SECRET".to_owned(),
                "-LEAK".to_owned(),
            ],
            ..GitHubConfig::default()
        };
        let err = cfg
            .resolved_token()
            .expect_err("a non-zero tokenCommand must error");
        let msg = err.to_string();
        assert!(
            msg.contains("failed"),
            "error must report the command failure, got: {msg}"
        );
        assert!(
            !msg.contains("SECRET-LEAK"),
            "error must NOT leak the helper's stderr, got: {msg}"
        );
    }

    #[test]
    fn resolved_token_errors_on_non_utf8_output() {
        // A helper emitting invalid UTF-8 is rejected outright, not lossily
        // coerced into a corrupt token that would 401 opaquely at the API.
        let cfg = GitHubConfig {
            token_command: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                "printf '\\xff\\xfe'".to_owned(),
            ],
            ..GitHubConfig::default()
        };
        let err = cfg
            .resolved_token()
            .expect_err("a non-UTF-8 tokenCommand output must error");
        assert!(
            err.to_string().contains("non-UTF-8"),
            "error must report non-UTF-8 output, got: {err}"
        );
    }

    #[test]
    fn resolved_token_errors_on_empty_output() {
        // A command that succeeds but prints nothing is an error: an empty
        // token would otherwise reach the API as a broken credential.
        let cfg = GitHubConfig {
            token_command: vec!["true".to_owned()],
            ..GitHubConfig::default()
        };
        let err = cfg
            .resolved_token()
            .expect_err("an empty tokenCommand output must error");
        assert!(
            err.to_string().contains("empty output"),
            "error must report empty output, got: {err}"
        );
    }

    #[test]
    fn resolved_token_errors_on_missing_binary() {
        // A tokenCommand whose binary is not on PATH is a clear config error
        // naming the missing binary, not a panic.
        let cfg = GitHubConfig {
            token_command: vec!["jj-vine-no-such-token-bin".to_owned()],
            ..GitHubConfig::default()
        };
        let err = cfg
            .resolved_token()
            .expect_err("a missing tokenCommand binary must error");
        assert!(
            err.to_string().contains("not found in PATH"),
            "error must name the missing binary, got: {err}"
        );
    }

    #[test]
    fn resolved_token_errors_on_timeout() {
        // The resolved_token timeout arm: a helper that overruns the (injected,
        // short) timeout surfaces as a config error naming the command, not a
        // hang. Uses the private injectable-timeout entry so the test needs no
        // real multi-second sleep.
        let cfg = GitHubConfig {
            token_command: vec!["sh".to_owned(), "-c".to_owned(), "sleep 30".to_owned()],
            ..GitHubConfig::default()
        };
        let err = cfg
            .resolved_token_with_timeout(core::time::Duration::from_millis(200))
            .expect_err("an overrunning tokenCommand must error");
        assert!(
            err.to_string().contains("timed out"),
            "error must report the timeout, got: {err}"
        );
    }

    #[test]
    fn resolved_token_trims_literal_token() {
        // The literal path trims too (symmetry with the command path): a literal
        // with a trailing newline must not reach the Authorization header verbatim.
        let cfg = GitHubConfig {
            token: "ghp_literal\n".to_owned(),
            ..GitHubConfig::default()
        };
        assert_eq!(
            cfg.resolved_token().expect("literal token must resolve"),
            "ghp_literal",
            "a literal token must be trimmed"
        );
    }

    #[test]
    fn resolved_token_whitespace_literal_falls_through_to_command() {
        // An all-whitespace literal is treated as absent, falling through to the
        // command rather than returning a blank token.
        let cfg = GitHubConfig {
            token: "   \n".to_owned(),
            token_command: vec!["printf".to_owned(), "cmd-tok".to_owned()],
            ..GitHubConfig::default()
        };
        assert_eq!(
            cfg.resolved_token()
                .expect("must fall through to the command"),
            "cmd-tok",
            "an all-whitespace literal must fall through to tokenCommand"
        );
    }

    #[test]
    fn detect_forge_from_host_github() {
        assert_eq!(
            ForgeType::detect_from_host("github.com"),
            Some(ForgeType::GitHub)
        );
        assert_eq!(
            ForgeType::detect_from_host("github.example.com"),
            Some(ForgeType::GitHub)
        );
    }

    #[test]
    fn detect_forge_from_host_gitlab() {
        assert_eq!(
            ForgeType::detect_from_host("gitlab.com"),
            Some(ForgeType::GitLab)
        );
        assert_eq!(
            ForgeType::detect_from_host("gitlab.example.com"),
            Some(ForgeType::GitLab)
        );
    }

    #[test]
    fn detect_forge_from_host_unknown() {
        assert_eq!(ForgeType::detect_from_host("git.example.com"), None);
        assert_eq!(ForgeType::detect_from_host("code.example.com"), None);
    }

    /// Set a repo-layer `jj-vine.<key>` value in the isolated test repo.
    fn set_repo(repo_path: &Path, key: &str, value: &str) {
        isolated_jj(repo_path)
            .exec(["config", "set", "--repo", key, value])
            .expect("Failed to set repo config");
    }

    /// Add a git remote to the isolated test repo.
    fn add_remote(repo_path: &Path, name: &str, url: &str) {
        isolated_jj(repo_path)
            .exec(["git", "remote", "add", name, url])
            .expect("Failed to add remote");
    }

    /// Seed the minimum for a loadable GitHub-forge config: `forge = github`
    /// and a token (so validation passes once `project` is present). `project`
    /// and `host` are left for the test to set explicitly or to derive.
    fn seed_github(repo_path: &Path) {
        set_repo(repo_path, "jj-vine.forge", "github");
        set_repo(repo_path, "jj-vine.github.token", "gh-test-token");
    }

    #[test]
    fn derive_explicit_project_wins() {
        let (_temp, repo_path) = create_test_repo();
        seed_github(&repo_path);
        set_repo(&repo_path, "jj-vine.github.project", "a/b");
        add_remote(&repo_path, "origin", "git@github.com:c/d.git");
        let config = load_isolated(&repo_path).expect("load");
        assert_eq!(config.github.project, "a/b", "repo-explicit project wins");
    }

    #[test]
    fn derive_project_and_host_when_unset() {
        let (_temp, repo_path) = create_test_repo();
        seed_github(&repo_path);
        add_remote(&repo_path, "origin", "git@github.com:c/d.git");
        let config = load_isolated(&repo_path).expect("load");
        assert_eq!(config.github.project, "c/d");
        assert_eq!(config.github.host, "https://api.github.com");
    }

    #[test]
    fn derive_host_when_unset() {
        let (_temp, repo_path) = create_test_repo();
        seed_github(&repo_path);
        set_repo(&repo_path, "jj-vine.github.project", "a/b");
        add_remote(&repo_path, "origin", "git@github.com:c/d.git");
        let config = load_isolated(&repo_path).expect("load");
        assert_eq!(
            config.github.host, "https://api.github.com",
            "host derives from the remote when the repo layer left it unset"
        );
    }

    #[test]
    fn derive_explicit_host_wins() {
        let (_temp, repo_path) = create_test_repo();
        seed_github(&repo_path);
        set_repo(
            &repo_path,
            "jj-vine.github.host",
            "https://ghe.example/api/v3",
        );
        add_remote(&repo_path, "origin", "git@github.com:c/d.git");
        let config = load_isolated(&repo_path).expect("load");
        assert_eq!(
            config.github.host, "https://ghe.example/api/v3",
            "repo-explicit host wins per ruling (b)"
        );
        assert_eq!(config.github.project, "c/d", "project still derives");
    }

    #[test]
    fn derive_supersedes_global_project_and_host_literal() {
        // The load-bearing distinction between the implemented precedence
        // (ruling (b): gate derivation on the *repo* layer's absence) and the
        // rejected one (ruling (a): gate on the *merged* config's absence). A
        // non-empty value present only in the user/global layer must still be
        // superseded by the clone-derived one — this is why every non-sealed
        // clone that inherits the global `project = "sealedsecurity/sealed"`
        // literal derives its own project instead of keeping the stale global.
        // Under ruling (a) this test fails (the global literal is non-empty in
        // the merged config, so nothing derives); under ruling (b) it passes.
        let (_temp, repo_path) = create_test_repo();
        seed_github(&repo_path);
        // Seed the user layer (the isolated config file `load_isolated` reads),
        // NOT the repo layer, so `jj config list --repo` never sees these.
        let user_config = repo_path
            .parent()
            .expect("test repo always has a parent temp dir")
            .join(ISOLATED_CONFIG);
        std::fs::write(
            &user_config,
            "[jj-vine.github]\nproject = \"stale/global\"\nhost = \"https://ghe-stale.example/api/v3\"\n",
        )
        .expect("Failed to seed user-layer github literal");
        add_remote(&repo_path, "origin", "git@github.com:c/d.git");
        let config = load_isolated(&repo_path).expect("load");
        assert_eq!(
            config.github.project, "c/d",
            "a global-layer project literal is superseded by the clone-derived \
             value (ruling (b)); ruling (a) would keep the stale global"
        );
        assert_eq!(
            config.github.host, "https://api.github.com",
            "a global-layer host literal is likewise superseded by the derived \
             host — the divergent stale global never survives"
        );
    }

    #[test]
    fn derive_detection_failure_falls_back_to_validate_error() {
        let (_temp, repo_path) = create_test_repo();
        seed_github(&repo_path);
        // No project, no remote → detection fails, validate() errors.
        let result = load_isolated(&repo_path);
        let Err(Error::Config { message, .. }) = result else {
            panic!("Expected Config error, got: {result:?}");
        };
        assert!(
            message.contains("auto-detection from the 'origin' remote found no GitHub owner/repo"),
            "extended validate message must mention attempted detection, got: {message}"
        );
    }

    #[test]
    fn derive_forge_gating_gitlab_unchanged() {
        let (_temp, repo_path) = create_test_repo();
        set_repo(&repo_path, "jj-vine.forge", "gitlab");
        set_repo(
            &repo_path,
            "jj-vine.gitlab.host",
            "https://gitlab.example.com",
        );
        set_repo(&repo_path, "jj-vine.gitlab.project", "g/p");
        set_repo(&repo_path, "jj-vine.gitlab.token", "glpat-x");
        add_remote(&repo_path, "origin", "git@github.com:c/d.git");
        let config = load_isolated(&repo_path).expect("load");
        assert_eq!(
            config.github.project, "",
            "a GitHub origin must not fill github.project when forge is gitlab"
        );
    }

    #[test]
    fn derive_respects_remote_name() {
        let (_temp, repo_path) = create_test_repo();
        seed_github(&repo_path);
        set_repo(&repo_path, "jj-vine.remoteName", "upstream");
        add_remote(&repo_path, "upstream", "git@github.com:canon/repo.git");
        let config = load_isolated(&repo_path).expect("load");
        assert_eq!(
            config.github.project, "canon/repo",
            "derivation reads the configured remote name, not origin"
        );
    }

    #[test]
    fn derive_explicit_empty_project_derives() {
        let (_temp, repo_path) = create_test_repo();
        seed_github(&repo_path);
        set_repo(&repo_path, "jj-vine.github.project", "");
        add_remote(&repo_path, "origin", "git@github.com:c/d.git");
        let config = load_isolated(&repo_path).expect("load");
        assert_eq!(
            config.github.project, "c/d",
            "a repo-local empty project is treated as absent and derives"
        );
    }

    #[test]
    fn derive_never_touches_target_project() {
        let (_temp, repo_path) = create_test_repo();
        seed_github(&repo_path);
        add_remote(&repo_path, "origin", "git@github.com:c/d.git");
        let config = load_isolated(&repo_path).expect("load");
        assert_eq!(
            config.github.target_project, "",
            "target_project is never derived; it stays as static config resolved it"
        );
    }

    #[test]
    fn derive_fork_workflow_fence_errors() {
        let (_temp, repo_path) = create_test_repo();
        seed_github(&repo_path);
        add_remote(&repo_path, "origin", "git@github.com:me/fork.git");
        add_remote(&repo_path, "upstream", "git@github.com:canon/repo.git");
        let result = load_isolated(&repo_path);
        let Err(Error::Config { message, .. }) = result else {
            panic!("Expected Config error from fork-workflow fence, got: {result:?}");
        };
        assert!(
            message.contains("auto-detection from the 'origin' remote found no GitHub owner/repo"),
            "fork-workflow fence must skip derivation and hit the validate error, got: {message}"
        );
    }
}
