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

/// Initialize Forgejo/Codeburg/Gitea-specific configuration.
#[expect(clippy::single_call_fn, reason = "important")]
pub fn init(repo_path: impl Into<PathBuf>, remotes: Option<&Remotes>) -> Result<()> {
    run(&repo_path.into(), remotes, &mut TerminalPrompts)
}

fn run(repo_path: &Path, remotes: Option<&Remotes>, prompts: &mut impl Prompts) -> Result<()> {
    let existing_host = get_config(repo_path, "jj-vine.forgejo.host");
    let existing_project = get_config(repo_path, "jj-vine.forgejo.project");
    let existing_target_project = get_config(repo_path, "jj-vine.forgejo.targetProject");
    let existing_token = get_config_redacted(repo_path, "jj-vine.forgejo.token");

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

    let forgejo_host = prompts.text(TextPrompt {
        label: "Forgejo/Codeburg/Gitea instance URL (e.g. https://codeberg.org)",
        key: "jj-vine.forgejo.host",
        default: default_host,
        initial_text: None,
        allow_empty: false,
        validate: Some(validate_https_host as Validator),
    })?;
    validate_https_host(&forgejo_host)
        .map_err(|message| -> crate::error::Error { crate::error::make_whatever!("{message}") })?;

    let forgejo_project = prompts.text(TextPrompt {
        label: "Forgejo/Codeburg/Gitea repository (owner/repo)",
        key: "jj-vine.forgejo.project",
        default: default_project,
        initial_text: None,
        allow_empty: false,
        validate: None,
    })?;

    let default_target_project = existing_target_project
        .or(remotes.and_then(|remote| remote.upstream.clone()))
        .or(remotes.map(|remote| remote.origin.clone()))
        .unwrap_or(forgejo_project.clone());
    let forgejo_target_project = prompts.text(TextPrompt {
        label: "Target repository for PRs (upstream, leave blank for same as source repository)",
        key: "jj-vine.forgejo.targetProject",
        default: Some(default_target_project),
        initial_text: None,
        allow_empty: true,
        validate: None,
    })?;

    let forgejo_token = if let Some(token) = existing_token {
        println!(
            "Using existing Personal Access Token. To update it, unset it with `jj config unset --repo jj-vine.forgejo.token` and re-run `jj-vine init`, or edit the repo config with `jj config edit --repo`."
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

        prompts.token(
            "Forgejo/Codeburg/Gitea Personal Access Token",
            "jj-vine.forgejo.token",
        )?
    };

    set_config(repo_path, "jj-vine.forgejo.host", &forgejo_host)?;
    set_config(repo_path, "jj-vine.forgejo.project", &forgejo_project)?;
    set_config(
        repo_path,
        "jj-vine.forgejo.targetProject",
        &forgejo_target_project,
    )?;
    set_config_redacted(repo_path, "jj-vine.forgejo.token", &forgejo_token)?;

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
    fn typed_http_forgejo_host_is_rejected_by_prompt_flow() {
        let (_directory, repo_path) = create_repo();
        let mut user = ScriptedUser::new(&[Some("http://codeberg.org")]);
        assert!(run(&repo_path, None, &mut user).is_err());
        assert_eq!(user.asked[0].0, "jj-vine.forgejo.host");
        assert!(user.asked[0].2);
    }

    #[test]
    fn http_derived_forgejo_host_default_is_rejected_by_prompt_flow() {
        let (_directory, repo_path) = create_repo();
        let remotes = Remotes {
            origin: "http://gitea.example.com/owner/repo.git".to_owned(),
            source_push_url: None,
            upstream: None,
            target_forge: crate::remote::parse_forge_url("http://gitea.example.com/owner/repo.git"),
        };
        let mut user = ScriptedUser::new(&[None]);
        assert!(run(&repo_path, Some(&remotes), &mut user).is_err());
        assert_eq!(user.asked[0].0, "jj-vine.forgejo.host");
        assert_eq!(user.asked[0].1.as_deref(), Some("http://gitea.example.com"));
    }
}
