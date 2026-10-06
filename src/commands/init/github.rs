use std::path::PathBuf;

use dialoguer::{Input, Password};
use owo_colors::OwoColorize as _;

use crate::{
    commands::init::{Remotes, get_config, set_config},
    error::Result,
    remote::parse_forge_url,
};

/// Initialize GitHub-specific configuration.
#[expect(clippy::single_call_fn, reason = "important")]
#[expect(
    clippy::too_many_lines,
    reason = "meh, only +9. hey, if you're revisiting this function, consider fixing this? :)"
)]
pub fn init(repo_path: impl Into<PathBuf>, remotes: Option<&Remotes>) -> Result<()> {
    let repo_path = repo_path.into();
    let existing_host = get_config(&repo_path, "jj-vine.github.host");
    let existing_project = get_config(&repo_path, "jj-vine.github.project");
    let existing_target_project = get_config(&repo_path, "jj-vine.github.targetProject");
    let existing_token = get_config(&repo_path, "jj-vine.github.token");
    let existing_token_command = get_config(&repo_path, "jj-vine.github.tokenCommand");

    let remotes = remotes.as_ref();
    let target_forge = remotes.and_then(|r| r.target_forge.as_ref());
    let (source_remote_project, target_remote_project) = github_remote_projects(remotes.copied());
    let default_host = github_default_host(existing_host, target_forge);
    let default_project = existing_project.or(source_remote_project);

    let github_host_input = Input::<String>::new().with_prompt(format!(
        "{} {}",
        "GitHub API URL (e.g. https://api.github.com)".bold(),
        "jj-vine.github.host".dimmed()
    ));
    let github_host_input = if target_forge.is_some_and(github_requires_https) {
        let input = if let Some(default_host) = default_host {
            github_host_input.default(default_host)
        } else {
            github_host_input
        };
        input.validate_with(|host: &String| validate_https_host(host))
    } else if let Some(default_host) = default_host {
        github_host_input.default(default_host)
    } else {
        github_host_input
    };
    let github_host = github_host_input.interact_text()?;

    let github_project = if let Some(project) = default_project {
        Input::<String>::new()
            .with_prompt(format!(
                "{} {}",
                "GitHub repository (owner/repo)".bold(),
                "jj-vine.github.project".dimmed()
            ))
            .default(project)
            .interact_text()?
    } else {
        Input::<String>::new()
            .with_prompt(format!(
                "{} {}",
                "GitHub repository (owner/repo)".bold(),
                "jj-vine.github.project".dimmed()
            ))
            .interact_text()?
    };

    let github_target_project = Input::<String>::new()
        .with_prompt(format!(
            "{} {}",
            "Target repository for PRs (upstream, leave blank for same as source repository)"
                .bold(),
            "jj-vine.github.targetProject".dimmed()
        ))
        .with_initial_text(
            existing_target_project
                .or(target_remote_project)
                .unwrap_or(github_project.clone()),
        )
        .allow_empty(true)
        .interact_text()?;

    let github_token = prompt_for_github_token(
        existing_token,
        existing_token_command.is_some(),
        &github_host,
    )?;

    set_config(&repo_path, "jj-vine.github.host", &github_host)?;
    set_config(&repo_path, "jj-vine.github.project", &github_project)?;
    if !github_target_project.is_empty() {
        set_config(
            &repo_path,
            "jj-vine.github.targetProject",
            &github_target_project,
        )?;
    }
    save_github_token(
        &repo_path,
        github_token.as_deref(),
        existing_token_command.is_some(),
    )?;

    Ok(())
}

fn github_remote_projects(remotes: Option<&Remotes>) -> (Option<String>, Option<String>) {
    let Some(remotes) = remotes else {
        return (None, None);
    };

    let source_url = remotes
        .source_push_url
        .as_deref()
        .unwrap_or(&remotes.origin);
    let source_project = parse_forge_url(source_url).map(|forge| forge.project);
    let target_url = remotes.upstream.as_deref().unwrap_or(&remotes.origin);
    let target_project = parse_forge_url(target_url).map(|forge| forge.project);

    (source_project, target_project)
}

fn github_default_host(
    existing_host: Option<String>,
    target_forge: Option<&crate::remote::DetectedForge>,
) -> Option<String> {
    existing_host.or_else(|| match target_forge {
        Some(forge) => forge.host.derived().map(str::to_owned),
        None => Some("https://api.github.com".to_owned()),
    })
}
fn github_requires_https(forge: &crate::remote::DetectedForge) -> bool {
    forge.host.derived().is_none()
}
fn validate_https_host(host: &str) -> core::result::Result<(), &'static str> {
    if host.starts_with("https://") {
        Ok(())
    } else {
        Err("Enter an HTTPS GitHub API URL")
    }
}

fn prompt_for_github_token(
    existing_token: Option<String>,
    token_command_configured: bool,
    github_host: &str,
) -> Result<Option<String>> {
    if token_command_configured {
        return Ok(None);
    }
    if let Some(token) = existing_token {
        println!(
            "Using existing Personal Access Token. Run `jj config set --repo jj-vine.github.token <token>` to update it."
        );
        return Ok(Some(token));
    }

    println!();
    println!("{}", "Personal Access Token required scopes:".yellow());
    println!(
        "  {} {}",
        "•".yellow(),
        "repo (for creating/updating pull requests)".dimmed()
    );
    println!();
    println!(
        "  {}",
        format!(
            "Create token at: {}/settings/tokens/new",
            if github_host == "https://api.github.com" {
                "https://github.com"
            } else {
                github_host.strip_suffix("/api/v3").unwrap_or(github_host)
            }
        )
        .dimmed()
    );
    println!();

    Ok(Some(
        Password::new()
            .with_prompt(format!(
                "{} {}",
                "GitHub Personal Access Token".bold(),
                "jj-vine.github.token".dimmed()
            ))
            .interact()?,
    ))
}

fn save_github_token(
    repo_path: impl Into<PathBuf>,
    token: Option<&str>,
    token_command_configured: bool,
) -> Result<()> {
    if token_command_configured {
        set_config(repo_path, "jj-vine.github.token", "")?;
    } else if let Some(token) = token {
        set_config(repo_path, "jj-vine.github.token", token)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::remote::parse_forge_url;

    #[test]
    fn clone_host_is_preserved_and_unknown_host_has_no_public_default() {
        let public_forge = parse_forge_url("git@github.com:owner/repo.git").expect("GitHub forge");
        assert_eq!(
            github_default_host(None, Some(&public_forge)),
            Some("https://api.github.com".to_owned())
        );
        let clone_forge =
            parse_forge_url("https://github.example.com/owner/repo.git").expect("clone forge");
        assert_eq!(
            github_default_host(None, Some(&clone_forge)),
            Some("https://github.example.com/api/v3".to_owned())
        );

        let unknown_forge =
            parse_forge_url("git@github-work:owner/repo.git").expect("GitHub SSH alias");
        assert_eq!(github_default_host(None, Some(&unknown_forge)), None);
        assert!(github_requires_https(&unknown_forge));
    }

    #[test]
    fn plaintext_enterprise_host_requires_explicit_api_host() {
        let forge = parse_forge_url("http://github.example.com/owner/repo.git")
            .expect("GitHub Enterprise remote");
        assert_eq!(github_default_host(None, Some(&forge)), None);
        assert!(github_requires_https(&forge));
        assert!(validate_https_host("https://github.example.com/api/v3").is_ok());
    }

    #[test]
    fn source_uses_push_project_and_target_uses_fetch_project() {
        let fetch_forge = parse_forge_url("git@github.com:target/repo.git").expect("fetch forge");
        let remotes = Remotes {
            origin: "git@github.com:target/repo.git".to_owned(),
            source_push_url: Some("git@github.com:source/repo.git".to_owned()),
            upstream: None,
            target_forge: Some(fetch_forge),
        };

        assert_eq!(
            github_remote_projects(Some(&remotes)),
            (
                Some("source/repo".to_owned()),
                Some("target/repo".to_owned())
            )
        );
    }

    #[test]
    fn target_project_uses_upstream_fetch_url_when_selected() {
        let remotes = Remotes {
            origin: "git@github.com:person/fork.git".to_owned(),
            source_push_url: None,
            upstream: Some("git@github.com:owner/repo.git".to_owned()),
            target_forge: None,
        };

        assert_eq!(
            github_remote_projects(Some(&remotes)),
            (
                Some("person/fork".to_owned()),
                Some("owner/repo".to_owned())
            )
        );
    }

    #[test]
    fn inherited_token_command_clears_inherited_literal_token() {
        let directory = tempfile::TempDir::new().expect("temp directory");
        let repo_path = directory.path().join("repo");
        let user_config_path = directory.path().join("user-config.toml");
        std::fs::create_dir_all(&repo_path).expect("create repo");
        std::fs::write(
            &user_config_path,
            "[jj-vine]\nforge = \"github\"\n\n[jj-vine.github]\nhost = \"https://api.github.com\"\nproject = \"owner/repo\"\ntoken = \"inherited-fixture-token\"\ntokenCommand = [\"printf\", \"command-token\"]\n",
        )
        .expect("write inherited token configuration");

        let jj = crate::jj::Jujutsu::new_isolated(&repo_path, &user_config_path).expect("jj");
        jj.exec(["git", "init", "--colocate"]).expect("init repo");
        for (key, value) in [
            ("jj-vine.forge", "github"),
            ("jj-vine.github.host", "https://api.github.com"),
            ("jj-vine.github.project", "owner/repo"),
            (
                "jj-vine.github.tokenCommand",
                "[\"printf\", \"command-token\"]",
            ),
        ] {
            jj.exec(["config", "set", "--repo", key, value])
                .expect("configure repo");
        }
        let token_command_configured = jj
            .exec_redacted(["config", "get", "jj-vine.github.tokenCommand"])
            .is_ok();
        assert!(token_command_configured);

        save_github_token(&repo_path, None, token_command_configured)
            .expect("override inherited literal token");

        let config = crate::config::Config::load(&repo_path).expect("load effective config");
        assert!(config.github.token.is_empty());
        assert_eq!(config.github.token_command, ["printf", "command-token"]);
    }

    #[test]
    fn token_command_clears_inherited_literal_token() {
        let directory = tempfile::TempDir::new().expect("temp directory");
        let repo_path = directory.path().join("repo");
        let user_config_path = directory.path().join("user-config.toml");
        std::fs::create_dir_all(&repo_path).expect("create repo");
        std::fs::write(
            &user_config_path,
            "[jj-vine.github]\ntoken = \"inherited-fixture-token\"\n",
        )
        .expect("write inherited user token");

        let jj = crate::jj::Jujutsu::new_isolated(&repo_path, &user_config_path).expect("jj");
        jj.exec(["git", "init", "--colocate"]).expect("init repo");
        for (key, value) in [
            ("jj-vine.forge", "github"),
            ("jj-vine.github.host", "https://api.github.com"),
            ("jj-vine.github.project", "owner/repo"),
            (
                "jj-vine.github.tokenCommand",
                "[\"printf\", \"command-token\"]",
            ),
        ] {
            jj.exec(["config", "set", "--repo", key, value])
                .expect("configure repository");
        }

        let token_command_configured = jj
            .exec_redacted(["config", "get", "jj-vine.github.tokenCommand"])
            .is_ok();
        let token = prompt_for_github_token(
            Some("inherited-fixture-token".to_owned()),
            token_command_configured,
            "https://api.github.com",
        )
        .expect("skip token prompt");
        save_github_token(&repo_path, token.as_deref(), token_command_configured)
            .expect("override inherited literal token");

        let effective_config = jj
            .exec_redacted(["config", "list"])
            .expect("read effective config");
        let effective_config: toml::Value =
            toml::from_str(&effective_config.stdout).expect("parse effective config");
        let github = effective_config
            .get("jj-vine")
            .and_then(|value| value.get("github"))
            .expect("GitHub config");
        assert_eq!(github.get("token").and_then(toml::Value::as_str), Some(""));
        assert_eq!(
            github.get("tokenCommand").and_then(toml::Value::as_array),
            Some(&vec![
                toml::Value::String("printf".to_owned()),
                toml::Value::String("command-token".to_owned())
            ])
        );

        let config = crate::config::Config::load(&repo_path).expect("load effective repo config");
        assert!(config.github.token.is_empty());
        assert_eq!(config.github.token_command, ["printf", "command-token"]);
        assert_eq!(token, None);
    }
}
