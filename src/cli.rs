use std::path::PathBuf;

use clap::{Parser, Subcommand};
use tracing::{Dispatch, Level, dispatcher};
use tracing_subscriber::{
    EnvFilter,
    fmt::SubscriberBuilder,
    layer::SubscriberExt as _,
    util::SubscriberInitExt as _,
};

use crate::{
    commands::{status::StatusCommandConfig, submit::SubmitCommandConfig},
    error::Result,
    output::{BufferedOutput, FlatOutput, InteractiveOutput, SyncOutput},
    tracing_formatter::PlainFormatter,
};

#[derive(Parser)]
#[command(name = "jj-vine")]
#[command(about = "GitLab MR submission tool for Jujutsu workflows", long_about = None, version)]
pub struct Cli {
    /// Repository path (defaults to current directory).
    #[arg(short = 'R', long, global = true)]
    pub repository: Option<PathBuf>,

    /// Enable verbose logging.
    #[arg(short = 'v', long, global = true, default_value_t = Cli::default_verbosity())]
    pub verbose: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    #[command(about = SubmitCommandConfig::help_long())]
    Submit(SubmitCommandConfig),

    /// Initialize jj-vine configuration for this repository.
    Init,

    /// Show status of tracked bookmarks and their MRs/PRs.
    Status(StatusCommandConfig),
}

#[expect(clippy::module_name_repetitions, reason = "it's fine")]
pub struct CliConfig<'a> {
    /// Repository path (defaults to current directory).
    pub repository: PathBuf,

    /// Output formatter.
    pub output: &'a SyncOutput,
}

impl Cli {
    #[must_use]
    pub fn default_verbosity() -> bool {
        std::env::var("RUST_LOG").is_ok_and(|v| !v.is_empty())
    }

    pub async fn run_stdout(&self) -> Result<()> {
        let can_have_interactive_output = match self.command {
            Commands::Submit(_) | Commands::Status(_) => true,
            Commands::Init => false,
        };

        let candidate = wants_interactive_output(self.verbose, can_have_interactive_output)
            .then(InteractiveOutput::new);
        let output = select_output(candidate);

        let filter = EnvFilter::builder()
            .with_default_directive(Level::INFO.into())
            .from_env_lossy();

        if self.verbose {
            tracing_subscriber::registry()
                .with(filter)
                .with(
                    tracing_subscriber::fmt::layer()
                        .event_format(PlainFormatter::new().with_level(true).with_timestamp(true)),
                )
                .init();
        } else {
            tracing_subscriber::registry()
                .with(filter)
                .with(tracing_subscriber::fmt::layer().event_format(PlainFormatter::new()))
                .init();
        }

        self.run(&output).await
    }

    pub async fn run_captured(&self) -> Result<String> {
        let buffered_output = Box::new(SyncOutput::Buffered(BufferedOutput::new()));

        // with_writer requires something with 'static, dunno how else to do this
        let buffered_output = &*Box::leak(buffered_output);

        let subscriber = SubscriberBuilder::default()
            .with_writer(move || buffered_output)
            .with_max_level(Level::INFO)
            .event_format(PlainFormatter::new())
            .finish();
        let dispatch = Dispatch::new(subscriber);

        // TODO doesn't really work with concurrency
        let _ = dispatcher::set_default(&dispatch);
        self.run(buffered_output).await?;

        let buffer = if let SyncOutput::Buffered(buffered) = buffered_output {
            buffered.get_buffer()
        } else {
            unreachable!();
        };

        Ok(strip_ansi::strip_str(&buffer).to_string())
    }

    /// # Panics
    ///
    /// Panics if the current directory is inaccessible.
    pub async fn run(&self, output: &SyncOutput) -> Result<()> {
        let repo_path = self.repository.as_ref().map_or_else(
            || std::env::current_dir().expect("Failed to get current directory"),
            Into::into,
        );

        let main_config = CliConfig {
            repository: repo_path.clone(),
            output,
        };

        match &self.command {
            Commands::Submit(options) => {
                crate::commands::submit::submit(options, &main_config).await?;
                Ok(())
            }
            Commands::Init => {
                crate::commands::init::init(&main_config)?;
                Ok(())
            }
            Commands::Status(options) => {
                crate::commands::status::status(options, &main_config).await?;
                Ok(())
            }
        }
    }
}

/// Whether the run may use a spinner: `verbose` forces flat logging, and a
/// command that shows no progress (e.g. `init`) is never interactive. Terminal
/// usability is left to indicatif via [`InteractiveOutput::is_hidden`], which
/// also catches `TERM=dumb` or an unset `TERM` on a real pty.
#[must_use]
fn wants_interactive_output(verbose: bool, can_have_interactive_output: bool) -> bool {
    !verbose && can_have_interactive_output
}

/// Keep the spinner only if indicatif will draw it. A hidden spinner (stderr
/// piped or captured, or `TERM=dumb`/unset) drops every `log_message`, so fall
/// back to flat logging, which stays visible via `tracing`.
fn select_output(candidate: Option<InteractiveOutput>) -> SyncOutput {
    match candidate {
        Some(interactive) if !interactive.is_hidden() => SyncOutput::Interactive(interactive),
        _ => SyncOutput::Flat(FlatOutput::new()),
    }
}

#[cfg(test)]
mod tests {
    use indicatif::{ProgressBar, ProgressDrawTarget};

    use super::{select_output, wants_interactive_output};
    use crate::output::{InteractiveOutput, SyncOutput};

    fn spinner_with(target: ProgressDrawTarget) -> InteractiveOutput {
        InteractiveOutput::with_spinner(ProgressBar::with_draw_target(None, target))
    }

    #[test]
    fn hidden_spinner_falls_back_to_flat_output() {
        let output = select_output(Some(spinner_with(ProgressDrawTarget::hidden())));
        assert!(matches!(output, SyncOutput::Flat(_)));
    }

    /// A terminal indicatif draws to; every write succeeds and is discarded.
    #[derive(Debug)]
    struct NullTerm;

    impl indicatif::TermLike for NullTerm {
        fn width(&self) -> u16 {
            80
        }
        fn move_cursor_up(&self, _: usize) -> std::io::Result<()> {
            Ok(())
        }
        fn move_cursor_down(&self, _: usize) -> std::io::Result<()> {
            Ok(())
        }
        fn move_cursor_right(&self, _: usize) -> std::io::Result<()> {
            Ok(())
        }
        fn move_cursor_left(&self, _: usize) -> std::io::Result<()> {
            Ok(())
        }
        fn write_line(&self, _: &str) -> std::io::Result<()> {
            Ok(())
        }
        fn write_str(&self, _: &str) -> std::io::Result<()> {
            Ok(())
        }
        fn clear_line(&self) -> std::io::Result<()> {
            Ok(())
        }
        fn flush(&self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn visible_spinner_stays_interactive() {
        let target = ProgressDrawTarget::term_like(Box::new(NullTerm));
        let output = select_output(Some(spinner_with(target)));
        assert!(matches!(output, SyncOutput::Interactive(_)));
    }

    #[test]
    fn no_candidate_uses_flat_output() {
        assert!(matches!(select_output(None), SyncOutput::Flat(_)));
    }

    #[test]
    fn interactive_only_when_eligible() {
        assert!(wants_interactive_output(false, true));
        assert!(!wants_interactive_output(true, true));
        assert!(!wants_interactive_output(false, false));
    }
}
