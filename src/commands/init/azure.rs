use std::path::PathBuf;

use dialoguer::{Input, Password};
use owo_colors::OwoColorize as _;

use crate::{
    commands::init::{Remotes, get_config, set_config},
    error::Result,
};

#[expect(clippy::too_many_lines, reason = "important")]
#[expect(clippy::single_call_fn, reason = "important")]
pub fn init(repo_path: impl Into<PathBuf>, remotes: Option<&Remotes>) -> Result<()> {
    let repo_path = repo_path.into();
    let existing_host = get_config(&repo_path, "jj-vine.azure.host");
    let existing_vssps_host = get_config(&repo_path, "jj-vine.azure.vsspsHost");
    let existing_project = get_config(&repo_path, "jj-vine.azure.project");
    let existing_target_project = get_config(&repo_path, "jj-vine.azure.targetProject");
    let existing_token = get_config(&repo_path, "jj-vine.azure.token");
    let existing_source_repository_name =
        get_config(&repo_path, "jj-vine.azure.sourceRepositoryName");
    let existing_target_repository_name =
        get_config(&repo_path, "jj-vine.azure.targetRepositoryName");

    let remotes = remotes.as_ref();
    let forge = match remotes {
        Some(Remotes {
            target_forge: Some(forge),
            ..
        }) => Some(forge),
        _ => None,
    };

    let host_requires_https = forge.is_some_and(|forge| forge.host.derived().is_none());
    let default_host = azure_default_host(existing_host, forge);

    let azure_host_input = Input::<String>::new().with_prompt(format!(
        "{} {}",
        "Azure DevOps API URL (e.g. https://dev.azure.com)".bold(),
        "jj-vine.azure.host".dimmed()
    ));
    let azure_host_input = if host_requires_https {
        let input = if let Some(default_host) = default_host {
            azure_host_input.default(default_host)
        } else {
            azure_host_input
        };
        input.validate_with(|host: &String| validate_https_host(host))
    } else if let Some(default_host) = default_host {
        azure_host_input.default(default_host)
    } else {
        azure_host_input
    };
    let azure_host = azure_host_input.interact_text()?;

    let default_vssps_host = existing_vssps_host.unwrap_or_else(|| {
        format!(
            "https://vssps.{}",
            azure_host.trim_start_matches("https://")
        )
    });

    let azure_vssps_host = Input::<String>::new()
        .with_prompt(format!(
            "{} {}",
            "Azure DevOps Security (VSSP) API URL (e.g. https://vssps.dev.azure.com). Used to look up other users for automatic review requests.".bold(),
            "jj-vine.azure.vsspsHost".dimmed()
        ))
        .allow_empty(true)
        .default(default_vssps_host)
        .interact_text()?;

    let default_project = existing_project.or(forge.map(|f| f.project.clone()));

    let azure_project = if let Some(project) = default_project {
        Input::<String>::new()
            .with_prompt(format!(
                "{} {}",
                "Azure DevOps project (organization/project)".bold(),
                "jj-vine.azure.project".dimmed()
            ))
            .default(project)
            .interact_text()?
    } else {
        Input::<String>::new()
            .with_prompt(format!(
                "{} {}",
                "Azure DevOps project (organization/project)".bold(),
                "jj-vine.azure.project".dimmed()
            ))
            .interact_text()?
    };

    let default_target_project = existing_target_project
        .or(remotes.and_then(|f| f.upstream.clone()))
        .or(remotes.map(|r| r.origin.clone()));

    let azure_target_project = Input::<String>::new()
        .with_prompt(format!(
            "{} {}",
            "Target project for PRs (upstream, leave blank for same as source project)".bold(),
            "jj-vine.azure.targetProject".dimmed()
        ))
        .default(default_target_project.unwrap_or_default())
        .interact_text()?;

    let default_source_repository_name =
        existing_source_repository_name.or(forge.and_then(|f| f.repository_name.clone()));

    let azure_source_repository_name = if let Some(repository_name) = default_source_repository_name
    {
        Input::<String>::new()
            .with_prompt(format!(
                "{} {}",
                "Name of the repository where branches are pushed (source/fork project)".bold(),
                "jj-vine.azure.sourceRepositoryName".dimmed()
            ))
            .default(repository_name)
            .interact_text()?
    } else {
        Input::<String>::new()
            .with_prompt(format!(
                "{} {}",
                "Name of the repository where branches are pushed (source/fork project)".bold(),
                "jj-vine.azure.sourceRepositoryName".dimmed()
            ))
            .interact_text()?
    };

    let default_target_repository_name =
        existing_target_repository_name.unwrap_or(azure_source_repository_name.clone());

    let azure_target_repository_name = Input::<String>::new()
        .with_prompt(format!(
            "{} {}",
            "Name of the repository where MRs/PRs are created (target/upstream project)".bold(),
            "jj-vine.azure.targetRepositoryName".dimmed()
        ))
        .default(default_target_repository_name)
        .interact_text()?;

    let azure_token = if let Some(token) = existing_token {
        println!(
            "Using existing Personal Access Token. Run `jj config set --repo jj-vine.azure.token <token>` to update it."
        );
        token
    } else {
        let (org, _project) = azure_project.split_once('/').unwrap();

        println!();
        println!(
            "  {}",
            format!(
                "Create token at: {}/{}/_usersSettings/tokens",
                azure_host.trim_end_matches('/'),
                org
            )
            .dimmed()
        );
        println!();

        Password::new()
            .with_prompt(format!(
                "{} {}",
                "Azure DevOps Personal Access Token".bold(),
                "jj-vine.azure.token".dimmed()
            ))
            .interact()?
    };

    set_config(&repo_path, "jj-vine.azure.host", &azure_host)?;
    set_config(&repo_path, "jj-vine.azure.vsspsHost", &azure_vssps_host)?;
    set_config(&repo_path, "jj-vine.azure.project", &azure_project)?;
    set_config(
        &repo_path,
        "jj-vine.azure.targetProject",
        &azure_target_project,
    )?;
    set_config(
        &repo_path,
        "jj-vine.azure.sourceRepositoryName",
        &azure_source_repository_name,
    )?;
    set_config(
        &repo_path,
        "jj-vine.azure.targetRepositoryName",
        &azure_target_repository_name,
    )?;
    set_config(&repo_path, "jj-vine.azure.token", &azure_token)?;

    Ok(())
}
fn azure_default_host(
    existing_host: Option<String>,
    forge: Option<&crate::remote::DetectedForge>,
) -> Option<String> {
    if forge.is_some_and(|forge| forge.host.derived().is_none()) {
        return existing_host.filter(|host| host.starts_with("https://"));
    }

    existing_host
        .or_else(|| forge.and_then(|forge| forge.host.derived().map(str::to_owned)))
        .or_else(|| forge.is_none().then(|| "https://dev.azure.com".to_owned()))
        .map(|host| match host.strip_prefix("https://ssh.") {
            Some(rest) => format!("https://{rest}"),
            None => host,
        })
}

fn validate_https_host(host: &str) -> core::result::Result<(), &'static str> {
    if host.starts_with("https://") {
        Ok(())
    } else {
        Err("Enter an HTTPS Azure DevOps API URL")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssh_alias_requires_an_explicit_https_host() {
        let forge = crate::remote::parse_forge_url("git@azure-work:organization/project/repo")
            .expect("Azure DevOps SSH alias");

        assert_eq!(forge.host.derived(), None);
        assert_eq!(azure_default_host(None, Some(&forge)), None);
        assert!(validate_https_host("http://dev.azure.com").is_err());
        assert!(validate_https_host("https://ado.example.com").is_ok());
    }

    #[test]
    fn no_remote_keeps_the_dev_azure_default() {
        assert_eq!(
            azure_default_host(None, None).as_deref(),
            Some("https://dev.azure.com")
        );
    }

    #[test]
    fn azure_ssh_endpoint_keeps_its_existing_web_host_mapping() {
        let forge =
            crate::remote::parse_forge_url("git@ssh.dev.azure.com:v3/organization/project/repo")
                .expect("Azure DevOps SSH URL");

        assert_eq!(
            azure_default_host(None, Some(&forge)).as_deref(),
            Some("https://dev.azure.com")
        );
    }
}
