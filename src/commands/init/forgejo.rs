use std::path::PathBuf;

use dialoguer::{Input, Password};
use owo_colors::OwoColorize as _;

use crate::{
    commands::init::{Remotes, get_config, set_config},
    error::Result,
};

/// Initialize Forgejo/Codeburg/Gitea-specific configuration.
#[expect(clippy::single_call_fn, reason = "important")]
pub fn init(repo_path: impl Into<PathBuf>, remotes: Option<&Remotes>) -> Result<()> {
    let repo_path = repo_path.into();
    let existing_host = get_config(&repo_path, "jj-vine.forgejo.host");
    let existing_project = get_config(&repo_path, "jj-vine.forgejo.project");
    let existing_target_project = get_config(&repo_path, "jj-vine.forgejo.targetProject");
    let existing_token = get_config(&repo_path, "jj-vine.forgejo.token");

    let remotes = remotes.as_ref();
    let forge = match remotes {
        Some(Remotes {
            target_forge: Some(forge),
            ..
        }) => Some(forge),
        _ => None,
    };

    let default_host = forgejo_default_host(existing_host, forge);
    let default_project = existing_project.or(forge.map(|f| f.project.clone()));

    let forgejo_host_input = Input::<String>::new().with_prompt(format!(
        "{} {}",
        "Forgejo/Codeburg/Gitea instance URL (e.g. https://codeberg.org)".bold(),
        "jj-vine.forgejo.host".dimmed()
    ));
    let forgejo_host_input = if let Some(default_host) = default_host {
        forgejo_host_input.default(default_host)
    } else {
        forgejo_host_input
    };
    let forgejo_host = forgejo_host_input
        .validate_with(|host: &String| validate_https_host(host))
        .interact_text()?;

    let forgejo_project = if let Some(project) = default_project {
        Input::<String>::new()
            .with_prompt(format!(
                "{} {}",
                "Forgejo/Codeburg/Gitea repository (owner/repo)".bold(),
                "jj-vine.forgejo.project".dimmed()
            ))
            .default(project)
            .interact_text()?
    } else {
        Input::<String>::new()
            .with_prompt(format!(
                "{} {}",
                "Forgejo/Codeburg/Gitea repository (owner/repo)".bold(),
                "jj-vine.forgejo.project".dimmed()
            ))
            .interact_text()?
    };

    let forgejo_target_project = Input::<String>::new()
        .with_prompt(format!(
            "{} {}",
            "Target repository for PRs (upstream, leave blank for same as source repository)"
                .bold(),
            "jj-vine.forgejo.targetProject".dimmed()
        ))
        .default(
            existing_target_project
                .or(remotes.and_then(|f| f.upstream.clone()))
                .or(remotes.map(|r| r.origin.clone()))
                .unwrap_or(forgejo_project.clone()),
        )
        .interact_text()?;

    let forgejo_token = if let Some(token) = existing_token {
        println!(
            "Using existing Personal Access Token. Run `jj config set --repo jj-vine.forgejo.token <token>` to update it."
        );
        token
    } else {
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
            format!("Create token at: {forgejo_host}/user/settings/applications").dimmed(),
        );
        println!();

        Password::new()
            .with_prompt(format!(
                "{} {}",
                "Forgejo/Codeburg/Gitea Personal Access Token".bold(),
                "jj-vine.forgejo.token".dimmed()
            ))
            .interact()?
    };

    set_config(&repo_path, "jj-vine.forgejo.host", &forgejo_host)?;
    set_config(&repo_path, "jj-vine.forgejo.project", &forgejo_project)?;
    set_config(
        &repo_path,
        "jj-vine.forgejo.targetProject",
        &forgejo_target_project,
    )?;
    set_config(&repo_path, "jj-vine.forgejo.token", &forgejo_token)?;

    Ok(())
}
fn forgejo_default_host(
    existing_host: Option<String>,
    forge: Option<&crate::remote::DetectedForge>,
) -> Option<String> {
    if forge.is_some_and(|forge| forge.host.derived().is_none()) {
        return existing_host.filter(|host| crate::config::is_https_host(host));
    }

    existing_host
        .or_else(|| forge.and_then(|forge| forge.host.derived().map(str::to_owned)))
        .or_else(|| forge.is_none().then(|| "https://codeberg.org".to_owned()))
}

fn validate_https_host(host: &str) -> core::result::Result<(), &'static str> {
    if crate::config::is_https_host(host) {
        Ok(())
    } else {
        Err("Enter an HTTPS Forgejo instance URL")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_alias_requires_an_explicit_https_host() {
        let forge = crate::remote::parse_forge_url("git@gitea-work:owner/repo.git")
            .expect("Forgejo SSH alias");

        assert_eq!(forge.host.derived(), None);
        assert_eq!(forgejo_default_host(None, Some(&forge)), None);
        assert_eq!(
            forgejo_default_host(Some("http://gitea.example.com".to_owned()), Some(&forge)),
            None
        );
        assert!(validate_https_host("http://forge.example.com").is_err());
        assert!(validate_https_host("https://forge.example.com").is_ok());
    }

    #[test]
    fn rejects_plaintext_remote_and_existing_host_defaults() {
        let forge = crate::remote::parse_forge_url("http://gitea.example.com/owner/repo.git")
            .expect("Forgejo HTTP remote");

        assert_eq!(forge.host.derived(), Some("http://gitea.example.com"));
        let remote_default = forgejo_default_host(None, Some(&forge));
        assert_eq!(remote_default.as_deref(), Some("http://gitea.example.com"));
        assert!(
            remote_default
                .as_ref()
                .is_some_and(|host| validate_https_host(host).is_err())
        );
        let edited_default =
            forgejo_default_host(Some("http://gitea.example.com".to_owned()), None);
        assert_eq!(edited_default.as_deref(), Some("http://gitea.example.com"));
        assert!(
            edited_default
                .as_ref()
                .is_some_and(|host| validate_https_host(host).is_err())
        );
        let https_forge =
            crate::remote::parse_forge_url("https://gitea.example.com/owner/repo.git")
                .expect("Forgejo HTTPS remote");
        assert_eq!(
            forgejo_default_host(
                Some("http://gitea.example.com".to_owned()),
                Some(&https_forge)
            ),
            Some("http://gitea.example.com".to_owned())
        );
        assert!(validate_https_host("http://gitea.example.com").is_err());
    }

    #[test]
    fn no_remote_keeps_the_codeberg_default() {
        assert_eq!(
            forgejo_default_host(None, None).as_deref(),
            Some("https://codeberg.org")
        );
    }
}
