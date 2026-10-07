use std::path::{Path, PathBuf};

use owo_colors::OwoColorize as _;

use crate::{
    commands::init::{
        Prompts,
        Remotes,
        TerminalPrompts,
        TextPrompt,
        Validator,
        get_config,
        get_config_redacted,
        set_config,
        set_config_redacted,
    },
    error::Result,
};

#[expect(clippy::single_call_fn, reason = "important")]
pub fn init(repo_path: impl Into<PathBuf>, remotes: Option<&Remotes>) -> Result<()> {
    run(&repo_path.into(), remotes, &mut TerminalPrompts)
}

#[expect(clippy::too_many_lines, reason = "important")]
fn run(repo_path: &Path, remotes: Option<&Remotes>, prompts: &mut impl Prompts) -> Result<()> {
    let existing_host = get_config(repo_path, "jj-vine.azure.host");
    let existing_vssps_host = get_config(repo_path, "jj-vine.azure.vsspsHost");
    let existing_project = get_config(repo_path, "jj-vine.azure.project");
    let existing_target_project = get_config(repo_path, "jj-vine.azure.targetProject");
    let existing_token = get_config_redacted(repo_path, "jj-vine.azure.token");
    let existing_source_repository_name =
        get_config(repo_path, "jj-vine.azure.sourceRepositoryName");
    let existing_target_repository_name =
        get_config(repo_path, "jj-vine.azure.targetRepositoryName");

    let remotes = remotes.as_ref();
    let forge = match remotes {
        Some(Remotes {
            target_forge: Some(forge),
            ..
        }) => Some(forge),
        _ => None,
    };

    let default_host = azure_default_host(existing_host, forge);

    let azure_host = prompts.text(TextPrompt {
        label: "Azure DevOps API URL (e.g. https://dev.azure.com)",
        key: "jj-vine.azure.host",
        default: default_host,
        initial_text: None,
        allow_empty: false,
        validate: Some(validate_https_host as Validator),
    })?;
    validate_https_host(&azure_host)
        .map_err(|message| -> crate::error::Error { crate::error::make_whatever!("{message}") })?;

    let azure_host_without_scheme = azure_host
        .get(..8)
        .filter(|scheme| scheme.eq_ignore_ascii_case("https://"))
        .and_then(|_| azure_host.get(8..))
        .unwrap_or(&azure_host);
    let default_vssps_host =
        existing_vssps_host.unwrap_or_else(|| format!("https://vssps.{azure_host_without_scheme}"));

    let azure_vssps_host = prompts.text(TextPrompt {
        label: "Azure DevOps Security (VSSP) API URL (e.g. https://vssps.dev.azure.com). Used to look up other users for automatic review requests.",
        key: "jj-vine.azure.vsspsHost",
        default: Some(default_vssps_host),
        initial_text: None,
        allow_empty: true,
        validate: Some(validate_optional_https_host as Validator),
    })?;
    validate_optional_https_host(&azure_vssps_host)
        .map_err(|message| -> crate::error::Error { crate::error::make_whatever!("{message}") })?;

    let default_project = existing_project.or(forge.map(|f| f.project.clone()));
    let azure_project = prompts.text(TextPrompt {
        label: "Azure DevOps project (organization/project)",
        key: "jj-vine.azure.project",
        default: default_project,
        initial_text: None,
        allow_empty: false,
        validate: None,
    })?;

    let default_target_project = existing_target_project
        .or(remotes.and_then(|remote| remote.upstream.clone()))
        .or(remotes.map(|remote| remote.origin.clone()));
    let azure_target_project = prompts.text(TextPrompt {
        label: "Target project for PRs (upstream, leave blank for same as source project)",
        key: "jj-vine.azure.targetProject",
        default: Some(default_target_project.unwrap_or_default()),
        initial_text: None,
        allow_empty: true,
        validate: None,
    })?;

    let default_source_repository_name =
        existing_source_repository_name.or(forge.and_then(|f| f.repository_name.clone()));
    let azure_source_repository_name = prompts.text(TextPrompt {
        label: "Name of the repository where branches are pushed (source/fork project)",
        key: "jj-vine.azure.sourceRepositoryName",
        default: default_source_repository_name,
        initial_text: None,
        allow_empty: false,
        validate: None,
    })?;

    let default_target_repository_name =
        existing_target_repository_name.unwrap_or(azure_source_repository_name.clone());
    let azure_target_repository_name = prompts.text(TextPrompt {
        label: "Name of the repository where MRs/PRs are created (target/upstream project)",
        key: "jj-vine.azure.targetRepositoryName",
        default: Some(default_target_repository_name),
        initial_text: None,
        allow_empty: false,
        validate: None,
    })?;

    let azure_token = if let Some(token) = existing_token {
        println!(
            "Using existing Personal Access Token. To update it, unset it with `jj config unset --repo jj-vine.azure.token` and re-run `jj-vine init`, or edit the repo config with `jj config edit --repo`."
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

        prompts.token("Azure DevOps Personal Access Token", "jj-vine.azure.token")?
    };

    set_config(repo_path, "jj-vine.azure.host", &azure_host)?;
    set_config(repo_path, "jj-vine.azure.vsspsHost", &azure_vssps_host)?;
    set_config(repo_path, "jj-vine.azure.project", &azure_project)?;
    set_config(
        repo_path,
        "jj-vine.azure.targetProject",
        &azure_target_project,
    )?;
    set_config(
        repo_path,
        "jj-vine.azure.sourceRepositoryName",
        &azure_source_repository_name,
    )?;
    set_config(
        repo_path,
        "jj-vine.azure.targetRepositoryName",
        &azure_target_repository_name,
    )?;
    set_config_redacted(repo_path, "jj-vine.azure.token", &azure_token)?;

    Ok(())
}
fn azure_default_host(
    existing_host: Option<String>,
    forge: Option<&crate::remote::DetectedForge>,
) -> Option<String> {
    if forge.is_some_and(|forge| forge.host.derived().is_none()) {
        return existing_host.filter(|host| crate::config::is_https_host(host));
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
    if crate::config::is_https_host(host) {
        Ok(())
    } else {
        Err("Enter an HTTPS Azure DevOps API URL")
    }
}

/// The VSSPS URL may be left blank; any value given must be HTTPS.
fn validate_optional_https_host(host: &str) -> core::result::Result<(), &'static str> {
    if host.trim().is_empty() {
        Ok(())
    } else {
        validate_https_host(host)
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
        assert_eq!(
            azure_default_host(Some("http://dev.azure.com".to_owned()), Some(&forge)),
            None
        );
        assert!(validate_https_host("http://dev.azure.com").is_err());
        assert!(validate_https_host("https://ado.example.com").is_ok());
    }

    #[test]
    fn http_remote_is_not_accepted_as_an_api_host() {
        let forge =
            crate::remote::parse_forge_url("http://dev.azure.com/organization/project/repo")
                .expect("Azure DevOps HTTP remote");

        assert_eq!(forge.host.derived(), Some("http://dev.azure.com"));
        assert_eq!(
            azure_default_host(None, Some(&forge)),
            Some("http://dev.azure.com".to_owned())
        );
        assert!(
            forge
                .host
                .derived()
                .is_some_and(|host| validate_https_host(host).is_err())
        );
    }

    #[test]
    fn edited_http_default_is_rejected_even_for_recognized_remote() {
        let forge =
            crate::remote::parse_forge_url("https://dev.azure.com/organization/project/repo")
                .expect("Azure DevOps HTTPS remote");
        let edited_default =
            azure_default_host(Some("http://dev.azure.com".to_owned()), Some(&forge));
        assert_eq!(edited_default.as_deref(), Some("http://dev.azure.com"));
        assert!(
            edited_default
                .as_ref()
                .is_some_and(|host| validate_https_host(host).is_err())
        );
        assert_eq!(
            azure_default_host(None, Some(&forge)).as_deref(),
            Some("https://dev.azure.com")
        );
    }

    #[test]
    fn only_accepts_https_security_api_hosts_or_blank() {
        assert!(validate_optional_https_host("http://vssps.dev.azure.com").is_err());
        assert!(validate_optional_https_host("https://vssps.dev.azure.com").is_ok());
        assert!(validate_optional_https_host("").is_ok());
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
    #[derive(Default)]
    struct ScriptedUser {
        answers: std::collections::VecDeque<Option<&'static str>>,
        asked: Vec<(String, Option<String>, bool)>,
    }

    impl ScriptedUser {
        fn new(answers: &[Option<&'static str>]) -> Self {
            Self {
                answers: answers.iter().copied().collect(),
                asked: Vec::new(),
            }
        }
    }

    impl Prompts for ScriptedUser {
        fn text(&mut self, prompt: TextPrompt<'_>) -> Result<String> {
            self.asked.push((
                prompt.key.to_owned(),
                prompt.default.clone(),
                prompt.validate.is_some(),
            ));
            let value = self
                .answers
                .pop_front()
                .expect("scripted answer")
                .map_or_else(|| prompt.default.unwrap_or_default(), str::to_owned);
            if value.is_empty() && !prompt.allow_empty {
                return Err(crate::error::make_whatever!("{} needs a value", prompt.key));
            }
            if let Some(validate) = prompt.validate {
                validate(&value).map_err(|message| -> crate::error::Error {
                    crate::error::make_whatever!("{message}")
                })?;
            }
            Ok(value)
        }

        fn token(&mut self, _label: &str, _key: &str) -> Result<String> {
            Ok("fixture-pat".to_owned())
        }
    }

    fn create_repo() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().expect("temp dir");
        let repo_path = directory.path().join("repo");
        std::fs::create_dir_all(&repo_path).expect("repo dir");
        crate::jj::Jujutsu::new(&repo_path)
            .expect("jj")
            .exec(["git", "init", "--colocate"])
            .expect("init repo");
        (directory, repo_path)
    }

    #[test]
    fn typed_http_azure_host_is_rejected_by_prompt_flow() {
        let (_directory, repo_path) = create_repo();
        let mut user = ScriptedUser::new(&[Some("http://dev.azure.com")]);
        assert!(run(&repo_path, None, &mut user).is_err());
        assert_eq!(user.asked[0].0, "jj-vine.azure.host");
        assert!(user.asked[0].2);
    }

    #[test]
    fn http_derived_azure_host_default_is_rejected_by_prompt_flow() {
        let (_directory, repo_path) = create_repo();
        let remotes = Remotes {
            origin: "http://dev.azure.com/org/project/repo".to_owned(),
            source_push_url: None,
            upstream: None,
            target_forge: crate::remote::parse_forge_url("http://dev.azure.com/org/project/repo"),
        };
        let mut user = ScriptedUser::new(&[None]);
        assert!(run(&repo_path, Some(&remotes), &mut user).is_err());
        assert_eq!(user.asked[0].0, "jj-vine.azure.host");
        assert_eq!(user.asked[0].1.as_deref(), Some("http://dev.azure.com"));
    }

    #[test]
    fn typed_http_vssps_host_is_rejected_by_prompt_flow() {
        let (_directory, repo_path) = create_repo();
        let mut user = ScriptedUser::new(&[
            Some("https://dev.azure.com"),
            Some("http://vssps.dev.azure.com"),
        ]);
        assert!(run(&repo_path, None, &mut user).is_err());
        assert_eq!(user.asked[1].0, "jj-vine.azure.vsspsHost");
        assert!(user.asked[1].2);
    }

    #[test]
    fn http_existing_vssps_host_default_is_rejected_by_prompt_flow() {
        let (_directory, repo_path) = create_repo();
        crate::jj::Jujutsu::new(&repo_path)
            .expect("jj")
            .exec([
                "config",
                "set",
                "--repo",
                "jj-vine.azure.vsspsHost",
                "http://vssps.dev.azure.com",
            ])
            .expect("set HTTP default");
        let mut user = ScriptedUser::new(&[Some("https://dev.azure.com"), None]);
        assert!(run(&repo_path, None, &mut user).is_err());
        assert_eq!(user.asked[1].0, "jj-vine.azure.vsspsHost");
        assert_eq!(
            user.asked[1].1.as_deref(),
            Some("http://vssps.dev.azure.com")
        );
    }

    #[test]
    fn vssps_default_strips_https_scheme_case_insensitively() {
        let (_directory, repo_path) = create_repo();
        let mut user = ScriptedUser::new(&[
            Some("HTTPS://dev.azure.com"),
            None,
            Some("org/project"),
            None,
            Some("repo"),
            None,
        ]);
        run(&repo_path, None, &mut user).expect("mixed-case HTTPS scheme");
        assert_eq!(
            user.asked[1].1.as_deref(),
            Some("https://vssps.dev.azure.com")
        );
    }
}
