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
    /// first element is the binary. Used only when `token` is empty. The
    /// trimmed stdout becomes the token. A non-zero exit or empty output is an
    /// error.
    #[serde(default)]
    pub token_command: Vec<String>,
}

/// Maximum wall-clock time to wait for a `tokenCommand` helper.
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

    /// Resolve the GitHub token from a literal or a configured command. A
    /// non-empty literal wins and is trimmed; otherwise the command's trimmed
    /// stdout is used. Errors do not include the command's stderr.
    pub fn resolved_token(&self) -> Result<String> {
        self.resolved_token_with_timeout(TOKEN_COMMAND_TIMEOUT)
    }

    /// Resolve the token with a caller-supplied timeout for deterministic
    /// tests.
    fn resolved_token_with_timeout(&self, timeout: core::time::Duration) -> Result<String> {
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

        let bin_path = which::which(bin).map_err(|_| {
            ConfigSnafu {
                message: "github.tokenCommand binary not found in PATH".to_owned(),
            }
            .build()
        })?;

        let mut command = std::process::Command::new(&bin_path);
        command.args(args);
        let Some(output) = crate::process::output_with_timeout(command, timeout)? else {
            return Err(ConfigSnafu {
                message: format!("github.tokenCommand timed out after {timeout:?}"),
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
}

impl Default for DescriptionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sync: false,
            diagram: DescriptionDiagramConfig::default(),
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

        // Repo- or workspace-explicit values win; otherwise clone-derived
        // values supersede global config. Empty values collapse into the
        // derive path.
        if config.forge == ForgeType::GitHub {
            let clone_keys = clone_layer_keys(jj);
            let clone_keys = clone_keys.as_deref();
            let project_set = clone_layer_nonempty(clone_keys, "project", &config.github.project);
            let host_set = clone_layer_nonempty(clone_keys, "host", &config.github.host);
            if (!project_set || !host_set)
                && let Some(detected) =
                    crate::remote::detect_project(jj, &config.remote_name, ForgeType::GitHub)
            {
                if !project_set {
                    config.github.project = detected.project;
                }
                // An SSH alias remote has no known host; keep the configured one.
                if !host_set && let Some(host) = detected.host {
                    config.github.host = host;
                }
            }
            if config.github.host.is_empty() {
                GITHUB_DEFAULT_API_HOST.clone_into(&mut config.github.host);
            }
        }

        config.validate()?;

        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
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

    /// A plaintext Enterprise remote derives neither value, so the token is
    /// never sent to an `http://` API host.
    #[test]
    fn derive_skips_http_enterprise_remote() {
        let (_temp, repo_path) = create_test_repo();
        seed_github_config(&repo_path);
        set_repo_config(&repo_path, "jj-vine.github.project", "configured/repo");
        add_git_remote(
            &repo_path,
            "origin",
            "http://github.example.com/owner/repo.git",
        );

        let config = load_isolated(&repo_path).expect("load config");

        assert_eq!(config.github.host, "https://api.github.com");
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
        };

        assert_eq!(config.target_project(), "myuser/myrepo");
        assert_eq!(config.source_project(), "myuser/myrepo");
        assert!(!config.is_fork_workflow());
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
}
