#![expect(clippy::print_stdout, reason = "allowed for init")]

use std::{collections::HashMap, path::PathBuf};

use dialoguer::{Input, Select};
use owo_colors::OwoColorize as _;
use serde::Deserialize;
use strum::VariantArray as _;

use crate::{
    cli::CliConfig,
    config::ForgeType,
    error::{Result, make_whatever},
    jj::Jujutsu,
    remote::{self, DetectedForge},
};

mod azure;
mod forgejo;
mod github;
mod gitlab;

#[derive(Debug, Clone)]
struct Remotes {
    origin: String,
    source_push_url: Option<String>,
    upstream: Option<String>,
    target_forge: Option<DetectedForge>,
}

/// Initialize jj-vine configuration for this repository.
#[expect(clippy::too_many_lines, reason = "important")]
pub fn init(cli_config: &CliConfig<'_>) -> Result<()> {
    println!("This will configure jj-vine for your repository.");
    println!(
        "{}",
        "Configuration will be stored in .jj/repo/config.toml".dimmed()
    );
    println!();

    let existing_forge: Option<ForgeType> =
        get_config(&cli_config.repository, "jj-vine.forge").and_then(|s| s.parse().ok());

    let jj = Jujutsu::new(&cli_config.repository)?;

    let remotes = detect_remotes(&jj)?;

    let forge_type = if let Some(existing) = existing_forge {
        existing
    } else if let Some(Remotes {
        target_forge: Some(forge),
        ..
    }) = remotes.as_ref()
    {
        forge.forge_type
    } else {
        let selection = Select::new()
            .with_prompt(format!(
                "{} {}",
                "Which code forge are you using?".bold(),
                "jj-vine.forge".dimmed()
            ))
            .items(ForgeType::VARIANTS.iter().map(ForgeType::display_name))
            .default(0)
            .interact()?;

        ForgeType::VARIANTS[selection]
    };

    set_config(
        &cli_config.repository,
        "jj-vine.forge",
        forge_type.to_string(),
    )?;

    let existing_remote_name = get_config(&cli_config.repository, "jj-vine.remoteName");
    let remote_name = Input::<String>::new()
        .with_prompt(format!(
            "{} {}",
            "Remote name".bold(),
            "jj-vine.remoteName".dimmed()
        ))
        .default(existing_remote_name.unwrap_or_else(|| "origin".to_owned()))
        .interact_text()?;

    let existing_default_branch = get_config(&cli_config.repository, "jj-vine.defaultBranch");
    let default_branch = Input::<String>::new()
        .with_prompt(format!(
            "{} {}",
            "Default branch".bold(),
            "jj-vine.defaultBranch".dimmed()
        ))
        .default(existing_default_branch.unwrap_or_else(|| "main".to_owned()))
        .interact_text()?;

    set_config(&cli_config.repository, "jj-vine.remoteName", &remote_name)?;
    set_config(
        &cli_config.repository,
        "jj-vine.defaultBranch",
        &default_branch,
    )?;

    match forge_type {
        ForgeType::GitLab => {
            gitlab::init(&cli_config.repository, remotes.as_ref())?;
        }
        ForgeType::GitHub => {
            github::init(&cli_config.repository, remotes.as_ref())?;
        }
        ForgeType::Forgejo => {
            forgejo::init(&cli_config.repository, remotes.as_ref())?;
        }
        ForgeType::AzureDevOps => {
            azure::init(&cli_config.repository, remotes.as_ref())?;
        }
    }

    let recommended_alias = match forge_type {
        ForgeType::GitLab | ForgeType::Forgejo | ForgeType::AzureDevOps => "pr",
        ForgeType::GitHub => "mr",
    };
    let recommendation = format!(
        "\nIt is useful to set up an alias for this command, such as {}! Run {} to set it up.",
        format!("jj {recommended_alias}").bold().magenta(),
        format!(
            r#"jj config set --user aliases.{recommended_alias} '["util", "exec", "--", "jj-vine"]'"#
        )
        .cyan()
    );

    let message = match toml::from_str::<JJConfig>(&jj.exec(["config", "list", "aliases"])?.stdout)
    {
        Ok(JJConfig {
            aliases: Some(aliases),
        }) => {
            let alias = aliases
                .iter()
                .find(|(_, args)| args.iter().any(|a| a.contains("jj-vine")));
            match alias {
                Some((alias, _)) => format!(
                    "Configuration complete! You can now use: {}.",
                    format!("jj {alias}").bold()
                )
                .green()
                .to_string(),
                None => format!(
                    "Configuration complete! You can now use: {}.{}",
                    "jj-vine submit".bold(),
                    recommendation
                )
                .green()
                .to_string(),
            }
        }
        _ => format!(
            "Configuration complete! You can now use: {}.{}",
            "jj-vine submit".bold(),
            recommendation
        )
        .green()
        .to_string(),
    };

    println!(
        "\n{} {}\n\nThere are more configuration options available. See all configuration options at https://codeberg.org/abrenneke/jj-vine#configuration.",
        "✓".green().bold(),
        message
    );

    Ok(())
}

#[derive(Debug, Clone, Deserialize)]
struct JJConfig {
    aliases: Option<HashMap<String, Vec<String>>>,
}

/// Get a configuration value using jj config get.
fn get_config(repo_path: impl Into<PathBuf>, key: &str) -> Option<String> {
    match Jujutsu::new(repo_path).ok()?.exec(["config", "get", key]) {
        Ok(output) => {
            let value = output.stdout.trim();
            if value.is_empty() {
                None
            } else {
                Some(value.to_owned())
            }
        }
        Err(_) => None,
    }
}

/// Set a configuration value using jj config set.
fn set_config(repo_path: impl Into<PathBuf>, key: &str, value: impl AsRef<str>) -> Result<()> {
    Jujutsu::new(repo_path)?.exec(["config", "set", "--repo", key, value.as_ref()])?;
    Ok(())
}
/// Unset a repository-level configuration value.
pub(super) fn unset_config(repo_path: impl Into<PathBuf>, key: &str) -> Result<()> {
    Jujutsu::new(repo_path)?.exec(["config", "unset", "--repo", key])?;
    Ok(())
}

/// Set a configuration value without exposing its arguments or output to trace
/// logs.
pub(super) fn set_config_redacted(
    repo_path: impl Into<PathBuf>,
    key: &str,
    value: &str,
) -> Result<()> {
    let value = toml::Value::String(value.to_owned()).to_string();
    Jujutsu::new(repo_path)?.exec_secret(["config", "set", "--repo", key, &value])?;
    Ok(())
}

/// Return the source layer for one effective configuration key.
pub(super) fn config_key_source(jj: &Jujutsu, key: &str) -> Option<String> {
    let output = jj
        .exec([
            "config",
            "list",
            "--template",
            r#"name ++ "\t" ++ source ++ "\n""#,
            key,
        ])
        .ok()?;
    output.stdout.lines().find_map(|line| {
        let (name, source) = line.split_once('\t')?;
        (name == key).then(|| source.to_owned())
    })
}

/// List config keys from the repo or workspace layer, without reading values.
pub(super) fn clone_layer_keys(jj: &Jujutsu, table: &str) -> Option<Vec<String>> {
    let output = jj
        .exec([
            "config",
            "list",
            "--template",
            r#"name ++ "\t" ++ source ++ "\n""#,
            table,
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

/// A non-empty config value is clone-explicit when its key comes from the
/// repo or workspace layer. Unknown source layers keep the value.
pub(super) fn clone_layer_nonempty(clone_keys: Option<&[String]>, key: &str, value: &str) -> bool {
    !value.is_empty() && clone_keys.is_none_or(|keys| keys.iter().any(|name| name == key))
}

/// Keep clone-explicit values, otherwise prefer a remote-derived default to
/// an inherited global value.
pub(super) fn derived_config_default(
    existing: Option<String>,
    derived: Option<String>,
    clone_keys: Option<&[String]>,
    key: &str,
) -> Option<String> {
    let explicit = existing
        .as_ref()
        .filter(|value| clone_layer_nonempty(clone_keys, key, value));
    explicit.cloned().or(derived).or(existing)
}

fn parse_init_remote_line(line: &str) -> Result<remote::RemoteListEntry<'_>> {
    remote::parse_remote_list_line(line)
        .ok_or_else(|| make_whatever!("Failed to parse remote line for <unknown>"))
}

fn detect_remotes(jj: &Jujutsu) -> Result<Option<Remotes>> {
    let output = jj.exec_redacted(["git", "remote", "list"])?;
    let remotes: HashMap<_, _> = output
        .stdout
        .lines()
        .map(parse_init_remote_line)
        .map(|entry| entry.map(|entry| (entry.name, entry)))
        .collect::<Result<_>>()?;

    let origin = remotes.get("origin");

    if let (Some(origin), Some(upstream)) = (origin, remotes.get("upstream")) {
        return Ok(Some(Remotes {
            origin: origin.fetch_url.to_owned(),
            source_push_url: origin.push_url.map(str::to_owned),
            target_forge: remote::parse_forge_url(upstream.fetch_url),
            upstream: Some(upstream.fetch_url.to_owned()),
        }));
    }

    if let (Some(origin), Some(fork)) = (origin, remotes.get("fork")) {
        return Ok(Some(Remotes {
            origin: fork.fetch_url.to_owned(),
            source_push_url: fork.push_url.map(str::to_owned),
            target_forge: remote::parse_forge_url(origin.fetch_url),
            upstream: Some(origin.fetch_url.to_owned()),
        }));
    }

    if let Some(origin) = origin {
        return Ok(Some(Remotes {
            target_forge: remote::parse_forge_url(origin.fetch_url),
            origin: origin.fetch_url.to_owned(),
            source_push_url: origin.push_url.map(str::to_owned),
            upstream: None,
        }));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use tempfile::TempDir;

    use super::*;

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

    fn create_test_repo() -> (TempDir, PathBuf) {
        let temp_dir = TempDir::new().expect("create temp directory");
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
    fn set_push_url(repo_path: &Path, name: &str, url: &str) {
        Jujutsu::new(repo_path)
            .expect("create Jujutsu instance")
            .exec(["git", "remote", "set-url", name, "--push", url])
            .expect("set push URL");
    }

    #[test]
    fn detect_remotes_keeps_fetch_target_and_push_source_urls() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "origin", "git@github.com:target/repo.git");
        set_push_url(&repo_path, "origin", "git@github.com:source/repo.git");

        let jj = Jujutsu::new(&repo_path).expect("jj");
        let remotes = detect_remotes(&jj)
            .expect("detect remotes")
            .expect("origin");

        assert_eq!(remotes.origin, "git@github.com:target/repo.git");
        assert_eq!(
            remotes.source_push_url.as_deref(),
            Some("git@github.com:source/repo.git")
        );
        assert_eq!(
            remotes.target_forge.expect("target forge").project,
            "target/repo"
        );
    }

    #[test]
    fn detect_remotes_selects_fork_as_source_and_origin_as_target() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "fork", "git@github.com:person/fork.git");
        add_remote(&repo_path, "origin", "git@github.com:owner/repo.git");

        let jj = Jujutsu::new(&repo_path).expect("jj");
        let remotes = detect_remotes(&jj)
            .expect("detect remotes")
            .expect("origin");

        assert_eq!(remotes.origin, "git@github.com:person/fork.git");
        assert_eq!(remotes.source_push_url, None);
        assert_eq!(
            remotes.upstream.as_deref(),
            Some("git@github.com:owner/repo.git")
        );
        assert_eq!(
            remotes.target_forge.expect("target forge").project,
            "owner/repo"
        );
    }

    #[test]
    fn detect_remotes_never_traces_remote_userinfo() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(
            &repo_path,
            "origin",
            "https://user:safe-fixture-token@github.com/owner/repo.git",
        );
        let jj = Jujutsu::new(&repo_path).expect("jj");

        let logs = CapturedLogs::default().capture(|| {
            let remotes = detect_remotes(&jj)
                .expect("detect remotes")
                .expect("origin");
            assert_eq!(
                remotes.origin,
                "https://user:safe-fixture-token@github.com/owner/repo.git"
            );
        });

        assert!(
            logs.contains("git remote list"),
            "trace capture is live: {logs}"
        );
        assert!(
            !logs.contains("safe-fixture-token"),
            "remote userinfo leaked: {logs}"
        );
    }

    #[test]
    fn malformed_remote_line_uses_fixed_redacted_diagnostic() {
        let error = parse_init_remote_line("user:fixture-token@evil <no URL> extra")
            .expect_err("malformed credential-shaped name must fail");
        assert!(error.to_string().contains("<unknown>"));
        assert!(!error.to_string().contains("fixture-token"));
        assert!(!error.to_string().contains("evil"));
    }

    #[test]
    fn detect_remotes_without_upstream_keeps_origin_as_source_and_target() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "origin", "git@github.com:owner/repo.git");

        let jj = Jujutsu::new(&repo_path).expect("jj");
        let remotes = detect_remotes(&jj)
            .expect("detect remotes")
            .expect("origin");

        assert_eq!(remotes.origin, "git@github.com:owner/repo.git");
        assert_eq!(remotes.source_push_url, None);
        assert_eq!(remotes.upstream, None);
        assert_eq!(
            remotes.target_forge.expect("target forge").project,
            "owner/repo"
        );
    }
}
