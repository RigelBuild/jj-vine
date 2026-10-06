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

fn parse_init_remote_line(line: &str) -> Result<(&str, &str)> {
    remote::parse_remote_list_line(line)
        .map(|entry| (entry.name, entry.fetch_url))
        .ok_or_else(|| {
            let name = line.split_whitespace().next().filter(|name| {
                !name.is_empty()
                    && name
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"._-/".contains(&byte))
            });
            make_whatever!(
                "Failed to parse remote line for {}",
                name.unwrap_or("<unknown>")
            )
        })
}

#[expect(clippy::single_call_fn, reason = "seems fine")]
fn detect_remotes(jj: &Jujutsu) -> Result<Option<Remotes>> {
    let output = jj.exec_redacted(["git", "remote", "list"])?;
    let remotes: HashMap<_, _> = output
        .stdout
        .lines()
        .map(parse_init_remote_line)
        .collect::<Result<_>>()?;

    let origin = remotes.get("origin");

    if let Some(upstream) = remotes.get("upstream")
        && let Some(origin) = origin
    {
        return Ok(Some(Remotes {
            origin: origin.to_string(),
            target_forge: remote::parse_forge_url(upstream),
            upstream: Some(upstream.to_string()),
        }));
    }

    if let Some(fork) = remotes.get("fork")
        && let Some(origin) = origin
    {
        return Ok(Some(Remotes {
            origin: fork.to_string(),
            target_forge: remote::parse_forge_url(origin),
            upstream: Some(origin.to_string()),
        }));
    }

    if let Some(origin) = origin {
        return Ok(Some(Remotes {
            target_forge: remote::parse_forge_url(origin),
            origin: origin.to_string(),
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
    fn detect_remotes_uses_fetch_url_when_push_url_differs() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "origin", "git@github.com:owner/repo.git");
        set_push_url(
            &repo_path,
            "origin",
            "git@github.com:attacker/push-destination.git",
        );

        let jj = Jujutsu::new(&repo_path).expect("jj");
        let remotes = detect_remotes(&jj)
            .expect("detect remotes")
            .expect("origin");

        assert_eq!(remotes.origin, "git@github.com:owner/repo.git");
        assert_eq!(
            remotes.target_forge.expect("target forge").project,
            "owner/repo"
        );
    }

    #[test]
    fn detect_remotes_selects_origin_or_upstream() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "origin", "git@github.com:person/fork.git");
        add_remote(&repo_path, "upstream", "git@github.com:owner/repo.git");

        let jj = Jujutsu::new(&repo_path).expect("jj");
        let remotes = detect_remotes(&jj)
            .expect("detect remotes")
            .expect("origin");

        assert_eq!(remotes.origin, "git@github.com:person/fork.git");
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
    fn detect_remotes_selects_fork_as_source_and_origin_as_target() {
        let (_temp, repo_path) = create_test_repo();
        add_remote(&repo_path, "fork", "git@github.com:person/fork.git");
        add_remote(&repo_path, "origin", "git@github.com:owner/repo.git");

        let jj = Jujutsu::new(&repo_path).expect("jj");
        let remotes = detect_remotes(&jj)
            .expect("detect remotes")
            .expect("origin");

        assert_eq!(remotes.origin, "git@github.com:person/fork.git");
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
    fn malformed_remote_line_names_only_safe_remote_identifier() {
        let error = parse_init_remote_line("origin <no URL> (push: git@github.com:o/r.git) extra")
            .expect_err("extra field must fail");
        assert!(error.to_string().contains("origin"));
        assert!(!error.to_string().contains("github.com"));

        let error = parse_init_remote_line("user:fixture-token@evil <no URL> extra")
            .expect_err("malformed credential-shaped name must fail");
        assert!(error.to_string().contains("<unknown>"));
        assert!(!error.to_string().contains("fixture-token"));
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
        assert_eq!(remotes.upstream, None);
        assert_eq!(
            remotes.target_forge.expect("target forge").project,
            "owner/repo"
        );
    }
}
