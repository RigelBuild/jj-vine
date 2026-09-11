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

        let output: SyncOutput =
            match interactive_output_candidate(self.verbose, can_have_interactive_output) {
                // Only keep the spinner if indicatif will actually draw it. On a
                // non-terminal stderr (piped/captured, or `TERM=dumb`/unset) the
                // spinner is hidden and every `log_message` through it is dropped,
                // so fall back to flat logging, which stays visible via `tracing`.
                Some(interactive) if !interactive.is_hidden() => {
                    SyncOutput::Interactive(interactive)
                }
                _ => SyncOutput::Flat(FlatOutput::new()),
            };

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

/// Build an interactive spinner candidate when the run is eligible for one, or
/// `None` when it must use flat logging regardless of the terminal.
///
/// This decides only the two conditions the caller controls: `verbose` forces
/// flat logging, and a command that shows no progress (e.g. `init`) is never
/// interactive. Whether stderr is actually a usable terminal is left to
/// `indicatif` — the caller checks [`InteractiveOutput::is_hidden`] on the
/// returned candidate. Deriving the terminal test here (e.g. only
/// `stderr().is_terminal()`) would miss cases `indicatif` still hides, such as
/// `TERM=dumb` or an unset `TERM` on a real pty, and the spinner's output would
/// vanish silently.
#[must_use]
fn interactive_output_candidate(
    verbose: bool,
    can_have_interactive_output: bool,
) -> Option<InteractiveOutput> {
    (!verbose && can_have_interactive_output).then(InteractiveOutput::new)
}

#[cfg(test)]
mod tests {
    use super::interactive_output_candidate;

    #[test]
    fn candidate_only_when_eligible() {
        // A command that supports a spinner, not verbose: eligible (the caller
        // still drops it if indicatif would hide it on a non-terminal).
        assert!(interactive_output_candidate(false, true).is_some());
        // Verbose always uses flat logging, even when a spinner is supported.
        assert!(interactive_output_candidate(true, true).is_none());
        // A command that cannot show a spinner (e.g. init) is never interactive.
        assert!(interactive_output_candidate(false, false).is_none());
    }
}
