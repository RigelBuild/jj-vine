use std::path::{Path, PathBuf};

use owo_colors::OwoColorize as _;

use crate::{
    commands::init::{
        Prompts,
        Remotes,
        TerminalPrompts,
        TextPrompt,
        Validator,
        clone_layer_keys,
        config_key_source,
        derived_config_default,
        set_config,
        set_config_redacted,
        unset_config,
    },
    config::ForgeType,
    error::{Error, Result, make_whatever},
    jj::Jujutsu,
    remote::{ApiHost, parse_forge_url, same_api_host},
};

/// Public GitHub API URL, offered only when no remote says otherwise.
const PUBLIC_API_HOST: &str = "https://api.github.com";

/// Initialize GitHub-specific configuration.
#[expect(clippy::single_call_fn, reason = "important")]
pub fn init(repo_path: impl Into<PathBuf>, remotes: Option<&Remotes>) -> Result<()> {
    let repo_path = repo_path.into();
    let jj = Jujutsu::new(&repo_path)?;
    run(&jj, &repo_path, remotes, &mut TerminalPrompts)
}

/// The wizard body. `jj` reads the effective configuration; answers come
/// from `prompts`.
fn run(
    jj: &Jujutsu,
    repo_path: &Path,
    remotes: Option<&Remotes>,
    prompts: &mut impl Prompts,
) -> Result<()> {
    let existing = ExistingSettings::read(jj)?;
    let derived = github_remote_defaults(remotes);
    if derived.unpaired {
        println!(
            "{}",
            "The push and fetch remotes are not on one known GitHub API host, so the API URL and \
             repositories are not derived from them."
                .yellow()
        );
    }

    let ExistingSettings {
        host,
        project,
        target_project,
        credential,
        token_command_source,
        invalid_token_command,
    } = existing;
    if invalid_token_command && token_command_source.as_deref() != Some("repo") {
        return Err(make_whatever!(
            "jj-vine.github.tokenCommand is malformed in a non-repository config layer; unset it there before saving a token"
        ));
    }
    let clone_keys = clone_layer_keys(jj, "jj-vine.github");
    let clone_keys = clone_keys.as_deref();

    let github_host = prompts.text(TextPrompt {
        label: "GitHub API URL (e.g. https://api.github.com)",
        key: "jj-vine.github.host",
        default: derived_config_default(
            host.filter(|host| crate::config::is_https_host(host)),
            derived.host.clone().filter(|_| remotes.is_some()),
            clone_keys,
            "jj-vine.github.host",
        )
        .or_else(|| remotes.is_none().then(|| PUBLIC_API_HOST.to_owned())),
        initial_text: None,
        allow_empty: false,
        validate: Some(validate_https_host as Validator),
    })?;
    validate_https_host(&github_host)
        .map_err(|message| -> Error { make_whatever!("{message}") })?;

    let github_project = prompts.text(TextPrompt {
        label: "GitHub repository (owner/repo)",
        key: "jj-vine.github.project",
        default: derived_config_default(
            project,
            derived.source_project.clone(),
            clone_keys,
            "jj-vine.github.project",
        ),
        initial_text: None,
        allow_empty: false,
        validate: None,
    })?;

    let github_target_project = prompts.text(TextPrompt {
        label: "Target repository for PRs (upstream, leave blank for same as source repository)",
        key: "jj-vine.github.targetProject",
        default: None,
        initial_text: Some(
            derived_config_default(
                target_project,
                derived.target_project.clone(),
                clone_keys,
                "jj-vine.github.targetProject",
            )
            .unwrap_or_else(|| github_project.clone()),
        ),
        allow_empty: true,
        validate: None,
    })?;

    let github_token = prompt_for_github_token(credential, &github_host, prompts)?;

    set_config(repo_path, "jj-vine.github.host", &github_host)?;
    set_config(repo_path, "jj-vine.github.project", &github_project)?;
    if !github_target_project.is_empty() {
        // GitHub repository names ignore ASCII case, but fork detection
        // compares exactly: store the same repository as the same string.
        let target = if github_target_project.eq_ignore_ascii_case(&github_project) {
            &github_project
        } else {
            &github_target_project
        };
        set_config(repo_path, "jj-vine.github.targetProject", target)?;
    }
    if let Some(token) = github_token {
        set_config_redacted(repo_path, "jj-vine.github.token", &token)?;
    }
    if invalid_token_command {
        unset_config(repo_path, "jj-vine.github.tokenCommand")?;
    }
    Ok(())
}

/// Which GitHub credential the effective configuration already provides.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExistingCredential {
    /// A non-empty literal `token`. It takes precedence over `tokenCommand`.
    Literal,
    /// No literal, and a `tokenCommand` that names a command.
    Command,
    /// No literal, and a `tokenCommand` that names no command.
    UnusableCommand,
    Missing,
}

/// Effective `jj-vine.github` settings that seed the wizard.
#[derive(Debug)]
struct ExistingSettings {
    host: Option<String>,
    project: Option<String>,
    target_project: Option<String>,
    credential: ExistingCredential,
    invalid_token_command: bool,
    token_command_source: Option<String>,
}
impl ExistingSettings {
    /// Read the effective `jj-vine.github` table. The table holds the token
    /// and the token command's arguments, so it is read redacted, and a
    /// parse error does not quote it.
    fn read(jj: &Jujutsu) -> Result<Self> {
        let output = jj.exec_redacted(["config", "list", "jj-vine.github"])?;
        let config: toml::Table = toml::from_str(&output.stdout).map_err(|_| -> Error {
            make_whatever!("Failed to parse the jj-vine.github config")
        })?;
        let github = config
            .get("jj-vine")
            .and_then(|value| value.get("github"))
            .and_then(toml::Value::as_table);
        let text = |key: &str| {
            github?
                .get(key)?
                .as_str()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        };

        let has_literal = text("token").is_some();
        let command = github.and_then(|github| github.get("tokenCommand"));
        let invalid_token_command = command.is_some_and(|value| !valid_token_command(value));
        let credential = match command {
            _ if has_literal => ExistingCredential::Literal,
            None => ExistingCredential::Missing,
            Some(command) if names_command(command) => ExistingCredential::Command,
            Some(_) => ExistingCredential::UnusableCommand,
        };

        Ok(Self {
            host: text("host"),
            project: text("project"),
            target_project: text("targetProject"),
            credential,
            invalid_token_command,
            token_command_source: invalid_token_command
                .then(|| config_key_source(jj, "jj-vine.github.tokenCommand"))
                .flatten(),
        })
    }
}

/// Whether a `tokenCommand` value has the schema type `Vec<String>`.
fn valid_token_command(command: &toml::Value) -> bool {
    command
        .as_array()
        .is_some_and(|argv| argv.iter().all(toml::Value::is_str))
}

/// Whether a valid `tokenCommand` names a usable binary.
fn names_command(command: &toml::Value) -> bool {
    valid_token_command(command)
        && command
            .as_array()
            .and_then(|argv| argv.first())
            .and_then(toml::Value::as_str)
            .is_some_and(|bin| !bin.trim().is_empty() && bin == bin.trim())
}
#[cfg(test)]
mod command_tests {
    use super::*;

    #[test]
    fn token_command_distinguishes_invalid_schema_from_unusable_argv() {
        assert!(valid_token_command(&toml::Value::Array(Vec::new())));
        assert!(!names_command(&toml::Value::Array(Vec::new())));
        assert!(!names_command(&toml::Value::Array(vec![
            toml::Value::String(" ".to_owned(),)
        ])));
        assert!(!names_command(&toml::Value::Array(vec![
            toml::Value::String(" gh ".to_owned(),)
        ])));
        assert!(names_command(&toml::Value::Array(vec![
            toml::Value::String("gh".to_owned(),)
        ])));
        assert!(!valid_token_command(&toml::Value::String("gh".to_owned())));
        assert!(!valid_token_command(&toml::Value::Array(vec![
            toml::Value::Integer(1)
        ])));
    }
}
/// Prompt defaults derived from the clone's remotes.
#[derive(Debug, Default, PartialEq, Eq)]
struct RemoteDefaults {
    host: Option<String>,
    /// Project branches are pushed to.
    source_project: Option<String>,
    /// Project PRs target, when it is another repository.
    target_project: Option<String>,
    /// The push and fetch remotes are GitHub URLs that cannot be shown to
    /// share one API host.
    unpaired: bool,
}

/// Derive defaults from the push (source) and fetch (target) URLs. Projects
/// are seeded only when both URLs are recognized GitHub URLs on one API
/// host, since one host serves both. A present remote that gives no API
/// host never defaults to the public API, which would receive the token.
fn github_remote_defaults(remotes: Option<&Remotes>) -> RemoteDefaults {
    let Some(remotes) = remotes else {
        return RemoteDefaults {
            host: Some(PUBLIC_API_HOST.to_owned()),
            ..RemoteDefaults::default()
        };
    };

    let source_url = remotes
        .source_push_url
        .as_deref()
        .unwrap_or(&remotes.origin);
    let target_url = remotes.upstream.as_deref().unwrap_or(&remotes.origin);
    let github =
        |url: &str| parse_forge_url(url).filter(|forge| forge.forge_type == ForgeType::GitHub);
    let (Some(source), Some(target)) = (github(source_url), github(target_url)) else {
        return RemoteDefaults::default();
    };

    let host = match (&source.host, &target.host) {
        (ApiHost::Derived(source_host), ApiHost::Derived(target_host))
            if same_api_host(source_host, target_host) =>
        {
            Some(target_host.clone())
        }
        _ if source_url == target_url => None,
        _ => {
            return RemoteDefaults {
                unpaired: true,
                ..RemoteDefaults::default()
            };
        }
    };

    let same_repository = target.project.eq_ignore_ascii_case(&source.project);
    RemoteDefaults {
        host,
        source_project: Some(source.project),
        target_project: (!same_repository).then_some(target.project),
        unpaired: false,
    }
}

fn validate_https_host(host: &str) -> core::result::Result<(), &'static str> {
    if crate::config::is_https_host(host) {
        Ok(())
    } else {
        Err("Enter an HTTPS GitHub API URL")
    }
}

/// Return a newly entered token to save, or `None` when the configuration
/// already provides one. A literal token wins over `tokenCommand`, as at
/// runtime, so it is kept as is.
fn prompt_for_github_token(
    credential: ExistingCredential,
    github_host: &str,
    prompts: &mut impl Prompts,
) -> Result<Option<String>> {
    match credential {
        ExistingCredential::Literal => {
            println!(
                "Using existing Personal Access Token. To update it, unset it with `jj config unset --repo jj-vine.github.token` and re-run `jj-vine init`, or edit the repo config with `jj config edit --repo`."
            );
            return Ok(None);
        }
        ExistingCredential::Command => return Ok(None),
        ExistingCredential::UnusableCommand => {
            println!(
                "{}",
                "jj-vine.github.tokenCommand names no command, so a Personal Access Token is \
                 needed."
                    .yellow()
            );
        }
        ExistingCredential::Missing => {}
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
            if github_host == PUBLIC_API_HOST {
                "https://github.com"
            } else {
                github_host.strip_suffix("/api/v3").unwrap_or(github_host)
            }
        )
        .dimmed()
    );
    println!();
    prompts
        .token("GitHub Personal Access Token", "jj-vine.github.token")
        .map(Some)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use tempfile::TempDir;

    use super::*;
    use crate::commands::init::detect_remotes;

    /// What the wizard asked, as the user saw it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Asked {
        key: String,
        default: Option<String>,
        initial_text: Option<String>,
        validated: bool,
    }

    /// A user who answers each prompt in turn. `None` presses Enter, which
    /// accepts the pre-filled text or else the default; `Some` is the whole
    /// final line. Input the terminal prompt would not accept, so it would
    /// ask again, is an error here.
    struct ScriptedUser {
        answers: VecDeque<Option<&'static str>>,
        tokens: VecDeque<&'static str>,
        asked: Vec<Asked>,
    }

    impl ScriptedUser {
        fn new(answers: &[Option<&'static str>], tokens: &[&'static str]) -> Self {
            Self {
                answers: answers.iter().copied().collect(),
                tokens: tokens.iter().copied().collect(),
                asked: Vec::new(),
            }
        }

        fn asked(&self, key: &str) -> Option<&Asked> {
            self.asked.iter().find(|asked| asked.key == key)
        }
    }

    impl Prompts for ScriptedUser {
        fn text(&mut self, prompt: TextPrompt<'_>) -> Result<String> {
            self.asked.push(Asked {
                key: prompt.key.to_owned(),
                default: prompt.default.clone(),
                initial_text: prompt.initial_text.clone(),
                validated: prompt.validate.is_some(),
            });
            let value = match self.answers.pop_front().expect("scripted answer") {
                Some(typed) => typed.to_owned(),
                None => prompt.initial_text.or(prompt.default).unwrap_or_default(),
            };
            if value.is_empty() && !prompt.allow_empty {
                return Err(make_whatever!("{} needs a value", prompt.key));
            }
            if let Some(validate) = prompt.validate {
                validate(&value).map_err(|message| -> Error { make_whatever!("{message}") })?;
            }
            Ok(value)
        }

        fn token(&mut self, _label: &str, key: &str) -> Result<String> {
            self.asked.push(Asked {
                key: key.to_owned(),
                default: None,
                initial_text: None,
                validated: false,
            });
            Ok(self.tokens.pop_front().expect("scripted token").to_owned())
        }
    }

    #[test]
    fn numeric_looking_pat_is_saved_as_a_string() {
        let (_directory, repo_path, jj) = create_repo(&[]);
        set_repo_config(&jj, "jj-vine.forge", "github");
        let mut user = ScriptedUser::new(&[None, Some("owner/repo"), None], &["12345"]);
        run_wizard(&jj, &repo_path, &mut user).expect("run wizard");
        let config = crate::config::Config::load(&repo_path).expect("load config");
        assert_eq!(config.github.token.as_str(), "12345");
    }

    #[test]
    fn insecure_existing_host_falls_back_to_derived_https_host() {
        let (_directory, repo_path, jj) =
            create_repo(&[("origin", "git@github.com:owner/repo.git", None)]);
        set_repo_config(
            &jj,
            "jj-vine.github.host",
            "http://insecure.example.com/api/v3",
        );
        let mut user = ScriptedUser::new(&[None, None, None], &["fixture-pat"]);
        run_wizard(&jj, &repo_path, &mut user).expect("use derived HTTPS host");
        assert_eq!(
            user.asked("jj-vine.github.host")
                .and_then(|prompt| prompt.default.as_deref()),
            Some("https://api.github.com")
        );
    }

    fn create_repo(remotes: &[(&str, &str, Option<&str>)]) -> (TempDir, PathBuf, Jujutsu) {
        let directory = TempDir::new().expect("temp directory");
        let repo_path = directory.path().join("repo");
        std::fs::create_dir_all(&repo_path).expect("create repo");
        let jj = Jujutsu::new(&repo_path).expect("jj");
        jj.exec(["git", "init", "--colocate"]).expect("init repo");
        for &(name, fetch_url, push_url) in remotes {
            jj.exec(["git", "remote", "add", name, fetch_url])
                .expect("add remote");
            if let Some(push_url) = push_url {
                jj.exec(["git", "remote", "set-url", name, "--push", push_url])
                    .expect("set push URL");
            }
        }
        (directory, repo_path, jj)
    }

    fn create_isolated_repo(
        remotes: &[(&str, &str, Option<&str>)],
        user_config: &str,
    ) -> (TempDir, PathBuf, Jujutsu) {
        let directory = TempDir::new().expect("temp directory");
        let repo_path = directory.path().join("repo");
        let config_path = directory.path().join("user-config.toml");
        std::fs::create_dir_all(&repo_path).expect("create repo");
        std::fs::write(&config_path, user_config).expect("write user config");
        let jj = Jujutsu::new_isolated(&repo_path, &config_path).expect("jj");
        jj.exec(["git", "init", "--colocate"]).expect("init repo");
        for &(name, fetch_url, push_url) in remotes {
            jj.exec(["git", "remote", "add", name, fetch_url])
                .expect("add remote");
            if let Some(push_url) = push_url {
                jj.exec(["git", "remote", "set-url", name, "--push", push_url])
                    .expect("set push URL");
            }
        }
        (directory, repo_path, jj)
    }

    fn set_repo_config(jj: &Jujutsu, key: &str, value: &str) {
        jj.exec(["config", "set", "--repo", key, value])
            .expect("set repo config");
    }

    /// Effective value of a config key, or `None` when it is unset.
    fn effective(jj: &Jujutsu, key: &str) -> Option<String> {
        jj.exec_redacted(["config", "get", key])
            .ok()
            .map(|output| output.stdout.trim().to_owned())
    }

    fn run_wizard(jj: &Jujutsu, repo_path: &Path, user: &mut ScriptedUser) -> Result<()> {
        let remotes = detect_remotes(jj).expect("detect remotes");
        run(jj, repo_path, remotes.as_ref(), user)
    }

    #[test]
    fn unrecognized_remote_requires_an_explicit_https_host() {
        for url in ["git@work:owner/repo.git", "git@github-work:owner/repo.git"] {
            let (_directory, repo_path, jj) = create_repo(&[("origin", url, None)]);

            let mut user = ScriptedUser::new(&[None], &[]);
            run_wizard(&jj, &repo_path, &mut user).expect_err("no public host default");
            let host = user.asked("jj-vine.github.host").expect("host prompt");
            assert_eq!(host.default, None, "{url}");
            assert!(host.validated, "{url}");

            let mut user = ScriptedUser::new(&[Some("http://github.example.com/api/v3")], &[]);
            run_wizard(&jj, &repo_path, &mut user).expect_err("plain HTTP host is rejected");

            let mut user = ScriptedUser::new(
                &[
                    Some("https://github.example.com/api/v3"),
                    Some("owner/repo"),
                    None,
                ],
                &["fixture-pat"],
            );
            run_wizard(&jj, &repo_path, &mut user).expect("explicit HTTPS host");
            assert_eq!(
                effective(&jj, "jj-vine.github.host").as_deref(),
                Some("https://github.example.com/api/v3")
            );
            assert_eq!(
                effective(&jj, "jj-vine.github.project").as_deref(),
                Some("owner/repo"),
                "{url}"
            );
        }
    }

    #[test]
    fn github_host_is_always_https_and_rejects_edited_defaults() {
        let (_directory, repo_path, jj) =
            create_repo(&[("origin", "git@github.com:owner/repo.git", None)]);
        set_repo_config(&jj, "jj-vine.github.host", "http://repo.example.com/api/v3");
        let mut user = ScriptedUser::new(&[None, None, None], &["fixture-pat"]);
        run_wizard(&jj, &repo_path, &mut user).expect("discard insecure stored host");
        assert_eq!(
            user.asked("jj-vine.github.host")
                .and_then(|prompt| prompt.default.as_deref()),
            Some("https://api.github.com")
        );

        let (_directory, repo_path, jj) = create_isolated_repo(
            &[],
            "[jj-vine.github]\nhost = \"http://global.example.com/api/v3\"\n",
        );
        let mut user = ScriptedUser::new(&[None, Some("owner/repo"), None], &["fixture-pat"]);
        run_wizard(&jj, &repo_path, &mut user).expect("discard insecure user host");
        assert_eq!(
            user.asked("jj-vine.github.host")
                .and_then(|prompt| prompt.default.as_deref()),
            Some(PUBLIC_API_HOST)
        );

        let (_directory, repo_path, jj) = create_isolated_repo(
            &[],
            "[jj-vine.github]\nhost = \"https://global.example.com/api/v3\"\n",
        );
        let mut user = ScriptedUser::new(&[Some("http://edited.example.com/api/v3")], &[]);
        run_wizard(&jj, &repo_path, &mut user).expect_err("edited HTTP host rejected");
        assert!(
            user.asked("jj-vine.github.host")
                .is_some_and(|prompt| prompt.validated)
        );
    }

    #[test]
    fn clone_derived_defaults_beat_global_values_but_repo_values_win() {
        let user_config = "[jj-vine.github]\nhost = \"https://global.example.com/api/v3\"\nproject = \"global/project\"\ntargetProject = \"global/target\"\n";
        let remotes = &[
            ("origin", "git@github.com:person/fork.git", None),
            ("upstream", "git@github.com:owner/repo.git", None),
        ][..];
        let (_directory, repo_path, jj) = create_isolated_repo(remotes, user_config);
        let mut user = ScriptedUser::new(&[None, None, None], &["fixture-pat"]);
        run_wizard(&jj, &repo_path, &mut user).expect("run wizard");
        assert_eq!(
            user.asked("jj-vine.github.host")
                .and_then(|prompt| prompt.default.as_deref()),
            Some(PUBLIC_API_HOST)
        );
        assert_eq!(
            user.asked("jj-vine.github.project")
                .and_then(|prompt| prompt.default.as_deref()),
            Some("person/fork")
        );
        assert_eq!(
            user.asked("jj-vine.github.targetProject")
                .and_then(|prompt| prompt.initial_text.as_deref()),
            Some("owner/repo")
        );

        assert_eq!(
            effective(&jj, "jj-vine.github.host").as_deref(),
            Some(PUBLIC_API_HOST)
        );
        assert_eq!(
            effective(&jj, "jj-vine.github.project").as_deref(),
            Some("person/fork")
        );
        assert_eq!(
            effective(&jj, "jj-vine.github.targetProject").as_deref(),
            Some("owner/repo")
        );
        for key in [
            "jj-vine.github.host",
            "jj-vine.github.project",
            "jj-vine.github.targetProject",
        ] {
            assert_eq!(
                config_key_source(&jj, key).as_deref(),
                Some("repo"),
                "{key}"
            );
        }
        set_repo_config(
            &jj,
            "jj-vine.github.host",
            "https://repo.example.com/api/v3",
        );
        set_repo_config(&jj, "jj-vine.github.project", "repo/source");
        set_repo_config(&jj, "jj-vine.github.targetProject", "repo/target");
        let mut user = ScriptedUser::new(&[None, None, None], &["fixture-pat"]);
        run_wizard(&jj, &repo_path, &mut user).expect("run wizard");
        assert_eq!(
            user.asked("jj-vine.github.host")
                .and_then(|prompt| prompt.default.as_deref()),
            Some("https://repo.example.com/api/v3")
        );
        assert_eq!(
            user.asked("jj-vine.github.project")
                .and_then(|prompt| prompt.default.as_deref()),
            Some("repo/source")
        );
        assert_eq!(
            user.asked("jj-vine.github.targetProject")
                .and_then(|prompt| prompt.initial_text.as_deref()),
            Some("repo/target")
        );
    }

    #[test]
    fn push_and_fetch_on_different_hosts_seed_nothing() {
        let (_directory, repo_path, jj) = create_repo(&[(
            "origin",
            "https://github.com/owner/repo.git",
            Some("https://github.example.com/me/repo.git"),
        )]);

        let mut user = ScriptedUser::new(
            &[
                Some("https://github.example.com/api/v3"),
                Some("me/repo"),
                None,
            ],
            &["fixture-pat"],
        );
        run_wizard(&jj, &repo_path, &mut user).expect("run wizard");

        let host = user.asked("jj-vine.github.host").expect("host prompt");
        assert_eq!(host.default, None);
        assert!(host.validated);
        let project = user
            .asked("jj-vine.github.project")
            .expect("project prompt");
        assert_eq!(project.default, None);
        let target = user
            .asked("jj-vine.github.targetProject")
            .expect("target prompt");
        assert_eq!(target.initial_text.as_deref(), Some("me/repo"));
        assert_eq!(
            effective(&jj, "jj-vine.github.targetProject").as_deref(),
            Some("me/repo")
        );
    }

    #[test]
    fn push_source_and_fetch_target_on_one_host_are_seeded() {
        let (_directory, repo_path, jj) = create_repo(&[(
            "origin",
            "git@github.com:owner/repo.git",
            Some("git@github.com:me/repo.git"),
        )]);

        let mut user = ScriptedUser::new(&[None, None, None], &["fixture-pat"]);
        run_wizard(&jj, &repo_path, &mut user).expect("run wizard");

        let host = user.asked("jj-vine.github.host").expect("host prompt");
        assert_eq!(host.default.as_deref(), Some(PUBLIC_API_HOST));
        assert!(host.validated);
        assert_eq!(
            effective(&jj, "jj-vine.github.project").as_deref(),
            Some("me/repo")
        );
        assert_eq!(
            effective(&jj, "jj-vine.github.targetProject").as_deref(),
            Some("owner/repo")
        );
        assert_eq!(
            effective(&jj, "jj-vine.github.token").as_deref(),
            Some("fixture-pat")
        );
    }

    #[test]
    fn same_repository_in_other_case_is_not_a_fork() {
        let (_directory, repo_path, jj) = create_repo(&[(
            "origin",
            "git@github.com:Owner/Repo.git",
            Some("git@github.com:owner/repo.git"),
        )]);
        set_repo_config(&jj, "jj-vine.forge", "github");
        set_repo_config(&jj, "jj-vine.github.token", "fixture-literal");

        let mut user = ScriptedUser::new(&[None, None, None], &[]);
        run_wizard(&jj, &repo_path, &mut user).expect("run wizard");
        let mut user = ScriptedUser::new(&[None, None, Some("OWNER/REPO")], &[]);
        run_wizard(&jj, &repo_path, &mut user).expect("rerun wizard");

        let config = crate::config::Config::load(&repo_path).expect("load config");
        assert_eq!(config.github.project, "owner/repo");
        assert!(!config.github.is_fork_workflow());
    }

    #[test]
    fn host_entry_derives_clone_host_and_never_public_for_unknown_hosts() {
        let defaults = |url: &str| {
            github_remote_defaults(Some(&Remotes {
                origin: url.to_owned(),
                source_push_url: None,
                upstream: None,
                target_forge: None,
            }))
        };

        assert_eq!(
            defaults("https://github.example.com/owner/repo.git").host,
            Some("https://github.example.com/api/v3".to_owned())
        );
        assert_eq!(
            github_remote_defaults(None).host.as_deref(),
            Some(PUBLIC_API_HOST)
        );
    }

    #[test]
    fn target_uses_upstream_fetch_url_when_selected() {
        let (_directory, _repo_path, jj) = create_repo(&[
            ("origin", "git@github.com:person/fork.git", None),
            ("upstream", "git@github.com:owner/repo.git", None),
        ]);
        let remotes = detect_remotes(&jj).expect("detect remotes");

        let defaults = github_remote_defaults(remotes.as_ref());
        assert_eq!(defaults.source_project.as_deref(), Some("person/fork"));
        assert_eq!(defaults.target_project.as_deref(), Some("owner/repo"));
    }

    #[test]
    fn empty_token_command_prompts_for_a_token() {
        let (_directory, repo_path, jj) = create_repo(&[]);
        set_repo_config(&jj, "jj-vine.github.tokenCommand", "[]");

        let mut user = ScriptedUser::new(&[None, Some("owner/repo"), None], &["fixture-pat"]);
        run_wizard(&jj, &repo_path, &mut user).expect("run wizard");

        assert!(user.asked("jj-vine.github.token").is_some());
        assert_eq!(
            effective(&jj, "jj-vine.github.token").as_deref(),
            Some("fixture-pat")
        );
    }

    #[test]
    fn literal_token_is_kept_over_token_command() {
        let directory = TempDir::new().expect("temp directory");
        let repo_path = directory.path().join("repo");
        let user_config_path = directory.path().join("user-config.toml");
        std::fs::create_dir_all(&repo_path).expect("create repo");
        std::fs::write(
            &user_config_path,
            "[jj-vine.github]\ntoken = \"user-literal-fixture\"\n",
        )
        .expect("write user config");
        let jj = Jujutsu::new_isolated(&repo_path, &user_config_path).expect("jj");
        jj.exec(["git", "init", "--colocate"]).expect("init repo");
        set_repo_config(
            &jj,
            "jj-vine.github.tokenCommand",
            "[\"printf\", \"command-token\"]",
        );

        let mut user = ScriptedUser::new(&[None, Some("owner/repo"), None], &[]);
        run(&jj, &repo_path, None, &mut user).expect("run wizard");

        assert!(user.asked("jj-vine.github.token").is_none());
        assert_eq!(
            effective(&jj, "jj-vine.github.token").as_deref(),
            Some("user-literal-fixture")
        );
    }

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

    #[test]
    fn malformed_repo_token_command_is_unset_when_pat_is_saved() {
        let (_directory, repo_path, jj) = create_repo(&[]);
        set_repo_config(&jj, "jj-vine.forge", "github");
        set_repo_config(&jj, "jj-vine.github.tokenCommand", "\"malformed\"");

        let mut user = ScriptedUser::new(&[None, Some("owner/repo"), None], &["fixture-pat"]);
        run_wizard(&jj, &repo_path, &mut user).expect("run wizard");

        assert!(effective(&jj, "jj-vine.github.tokenCommand").is_none());
        assert_eq!(
            effective(&jj, "jj-vine.github.token").as_deref(),
            Some("fixture-pat")
        );
        crate::config::Config::load(&repo_path).expect("configuration remains loadable");
    }

    #[test]
    fn malformed_non_repo_token_command_fails_before_saving_pat() {
        let (_directory, repo_path, jj) = create_isolated_repo(
            &[],
            "[jj-vine]\nforge = \"github\"\n[jj-vine.github]\ntokenCommand = \"malformed\"\n",
        );

        let mut user = ScriptedUser::new(&[None, Some("owner/repo"), None], &["fixture-pat"]);
        let error =
            run_wizard(&jj, &repo_path, &mut user).expect_err("must not write repo override");

        assert!(error.to_string().contains("non-repository config layer"));
        assert!(effective(&jj, "jj-vine.github.token").is_none());
        assert_eq!(
            effective(&jj, "jj-vine.github.tokenCommand").as_deref(),
            Some("malformed")
        );
    }
    #[test]
    fn empty_user_token_command_with_literal_token_completes_init() {
        let (_directory, repo_path, jj) = create_isolated_repo(
            &[],
            "[jj-vine]\nforge = \"github\"\n[jj-vine.github]\ntoken = \"fixture-literal\"\ntokenCommand = []\n",
        );
        let mut user = ScriptedUser::new(&[None, Some("owner/repo"), None], &[]);
        run(&jj, &repo_path, None, &mut user).expect("literal token permits init");
        assert!(user.asked("jj-vine.github.token").is_none());
        assert_eq!(
            effective(&jj, "jj-vine.github.token").as_deref(),
            Some("fixture-literal")
        );
    }

    #[test]
    fn token_command_is_used_without_tracing_its_argv() {
        let (_directory, repo_path, jj) = create_repo(&[]);
        set_repo_config(
            &jj,
            "jj-vine.github.tokenCommand",
            "[\"printf\", \"argv-fixture-secret\"]",
        );
        set_repo_config(&jj, "jj-vine.github.token", "");

        let mut user = ScriptedUser::new(&[None, Some("owner/repo"), None], &[]);
        let logs = CapturedLogs::default().capture(|| {
            run_wizard(&jj, &repo_path, &mut user).expect("run wizard");
        });

        assert!(user.asked("jj-vine.github.token").is_none());
        assert!(
            logs.contains("config list jj-vine.github"),
            "trace capture is live: {logs}"
        );
        assert!(
            !logs.contains("argv-fixture-secret"),
            "token command argv leaked: {logs}"
        );
        assert_eq!(effective(&jj, "jj-vine.github.token").as_deref(), Some(""));
    }

    #[test]
    fn pat_write_does_not_trace_the_token_argument() {
        let (_directory, repo_path, jj) = create_repo(&[]);
        let mut user = ScriptedUser::new(
            &[None, Some("owner/repo"), None],
            &["pat-write-fixture-secret"],
        );
        let logs = CapturedLogs::default().capture(|| {
            run_wizard(&jj, &repo_path, &mut user).expect("run wizard");
        });
        assert!(
            logs.contains("Running jj command: jj <redacted>"),
            "trace capture includes a redacted command: {logs}"
        );
        assert!(
            !logs.contains("pat-write-fixture-secret"),
            "PAT leaked into trace output: {logs}"
        );
    }
}
