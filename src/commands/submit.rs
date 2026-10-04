#![expect(clippy::module_name_repetitions, reason = "seems fine")]
use core::fmt::Write as _;
use std::{
    borrow::Cow,
    collections::{HashMap, HashSet},
};

use clap::Args;
use cli_table::{
    Cell as _,
    Table as _,
    format::{Border, Separator},
};
use itertools::Itertools as _;
use owo_colors::OwoColorize as _;
use snafu::{ensure_whatever, whatever};
use tracing::warn;
use unicode_segmentation::UnicodeSegmentation as _;

use crate::{
    bookmark::{BookmarkGraph, BookmarkOrPending, JJName},
    cli::CliConfig,
    commands::{GetBookmarksOptions, StrVisualWidth as _},
    config::{Config, ForgeType},
    description::FormatMergeRequest as _,
    error::{AggregateSnafu, Result},
    forge::ForgeImpl,
    jj::Jujutsu,
    output::Output as _,
    submit::{
        PlanContext,
        RootExecuteContext,
        execute::{self, MRUpdate, MRUpdateType},
        find_changes_to_submit,
        plan,
        stack_link::{self, StackLinkOutcome},
    },
};

#[derive(Args, Default)]
pub struct SubmitCommandConfig {
    /// Options for the revset.
    #[command(flatten)]
    pub revset_options: SubmitCommandRevsetOptions,

    /// The remote to push to. Defaults to the `git.push` or `git.fetch`
    /// settings.
    #[arg(long)]
    pub remote: Option<String>,

    /// Don't actually modify any merge requests or push bookmarks, only print
    /// what would be done.
    #[arg(long)]
    pub dry_run: bool,

    /// Use the built-in `jj git push` command instead of the configured push
    /// command.
    #[arg(long)]
    pub no_hooks: bool,

    /// Show the submission plan and do not execute it.
    #[arg(long, conflicts_with = "dry_run")]
    pub show_plan: bool,

    /// Create bookmarks for changes that don't have one (via `jj git push -c`).
    /// A bookmark will be created for each revision in the revset for this
    /// parameter, intersected with the main revset being submitted
    /// ([create] & [revset]). If `--create` is passed without a value, the
    /// value will default to `all()`, which will create one bookmark for each
    /// revision in the revset being submitted, if it does not already have one.
    ///
    /// Revisions will be skipped if they already have a bookmark, tracked or
    /// not.
    ///
    /// For example, if you have revision A off of `trunk()`, and B and C off of
    /// A, then:
    ///
    /// `jj-vine submit 'trunk()..' -c` -> creates bookmarks for A, B, and
    /// C.
    ///
    /// `jj-vine submit 'trunk()..' -c C` -> only creates a bookmark for C, so
    /// the pull/merge request for C contains both A and C. A is not pushed
    /// nor a pull or merge request created.
    ///
    /// You may also not specify a revset, and just use -c, in which case the
    /// value of -c will be used as the submitting revset. For example,
    /// `jj-vine submit -c @` is equivalent to `jj git push -c @ && jj-vine
    /// submit @` or `jj-vine submit -r @ -c @`.
    #[arg(short = 'c', long = "create", num_args=0..=1, require_equals=true, default_missing_value="all()")]
    pub create: Option<String>,
}

impl SubmitCommandConfig {
    fn to_get_bookmarks_options(&self) -> Result<GetBookmarksOptions> {
        match (
            self.revset_options.revset_positional.as_deref(),
            self.revset_options.revset.as_deref(),
            self.revset_options.tracked,
            self.create.as_deref(),
        ) {
            (Some(revset), None, false, _) | (None, Some(revset), false, _) => {
                Ok(GetBookmarksOptions::Revset(revset.to_owned()))
            }
            (None, None, true, _) => Ok(GetBookmarksOptions::Tracked),

            // Fall back to the same as -c if none of the other options are set
            (None, None, false, Some(create)) => Ok(GetBookmarksOptions::Revset(create.to_owned())),
            _ => {
                whatever!(
                    "You must specify a revset to submit with a positional argument, with the -r option, or with the --tracked option. You can also use the -c option to create bookmarks for changes that don't have one."
                );
            }
        }
    }

    #[must_use]
    pub fn help_long() -> String {
        format!(
            "
Submit one or more bookmarks to the code forge.
This command will create merge requests that don't exist, update
existing merge requests to the correct target branch, and sync all
merge request descriptions.

{}

Submit a single bookmark:
{}

Submit all tracked bookmarks:
{}

Preview submitting a revset without making changes:
{}
",
            "Examples:".yellow().bold(),
            "jj vine submit <bookmark>".green().bold(),
            "jj vine submit --tracked".green().bold(),
            "jj vine submit -r <revset> --dry-run".green().bold(),
        )
        .trim()
        .to_owned()
    }
}

#[derive(Args, Default)]
#[group(required = false, multiple = false)]
pub struct SubmitCommandRevsetOptions {
    /// The revset to submit (may use -r or not).
    #[arg(id = "revset")]
    pub revset_positional: Option<String>,

    /// The revset to submit (may use -r or not).
    #[arg(id = "revset_arg", short = 'r', long)]
    pub revset: Option<String>,

    /// Submit all tracked bookmarks.
    ///
    /// While this is roughly equivalent to
    /// `(mine() & tracked_remote_bookmarks()) ~ trunk()`, it includes the
    /// additional stipulation that all submitted bookmarks must be already
    /// pushed to the remote. Bookmarks which have non-tracked parents or
    /// children will be skipped over.
    #[arg(short = 't', long)]
    pub tracked: bool,
}

/// # Panics
///
/// Can panic for many reasons.
#[expect(clippy::too_many_lines, reason = "important")]
// #[expect(clippy::cognitive_complexity, reason = "main logic, it's fine")]
pub async fn submit(config: &SubmitCommandConfig, cli_config: &CliConfig<'_>) -> Result<()> {
    let jj = Jujutsu::new(&cli_config.repository)?;
    let mut output = cli_config.output;

    let repo_config = Config::load(&cli_config.repository)?;

    if let Some(mut fetch_args) = repo_config.fetch.to_args() {
        if let Some(remote) = config.remote.as_deref() {
            fetch_args.extend_from_slice(&["--remote", remote]);
        }

        if config.dry_run {
            output.log_message(&format!(
                "Would run `jj {}` before planning (note that the plan may change based on newly fetched data!)",
                fetch_args.join(" "),
            ));
        } else {
            jj.exec(fetch_args)?;
        }
    }

    let revset = config.to_get_bookmarks_options()?.to_revset();

    let mut pending_bookmarks = HashSet::new();
    if let Some(create) = config.create.as_deref() {
        output.log_current("Creating and pushing bookmarks");

        let changes_to_create_bookmarks_for = jj.log(format!("({create}) & ({revset})"))?;

        if changes_to_create_bookmarks_for.is_empty() {
            whatever!(
                "Your change parameter resolved to a revset ({}) & ({}), which is empty. This is probably not what you intended, as no bookmarks would be created. Not continuing with submit.",
                create,
                revset
            );
        }

        pending_bookmarks.extend(
            changes_to_create_bookmarks_for
                .iter()
                .filter(|c| c.bookmarks.is_empty())
                .map(|c| c.change_id.clone()),
        );
    }

    let changes = jj.log_with_pending_bookmarks(&revset, &pending_bookmarks)?;

    let bookmarks: Vec<_> = BookmarkOrPending::from_changes(&changes)
        .into_iter()
        .collect();

    ensure_whatever!(!bookmarks.is_empty(), "No bookmarks in revset {}", revset);

    output.log_message(&format!(
        "Submitting bookmarks{}: {}",
        if config.dry_run {
            " (dry run)"
        } else if config.show_plan {
            " (plan only)"
        } else {
            ""
        },
        bookmarks
            .iter()
            .map(|bookmark| bookmark.magenta().to_string())
            .join(", ")
    ));

    let changes = find_changes_to_submit(
        &jj,
        bookmarks.iter().map(BookmarkOrPending::change_id),
        literal_bookmark_targets(&revset, &bookmarks),
        &pending_bookmarks,
    )?;

    ensure_whatever!(
        !changes.is_empty(),
        "Resolved bookmark(s) {} but found no changes to submit — the named bookmark(s) may already be merged into trunk (inspect with `jj log -r <bookmark>`). For stacked submissions, confirm the expected commits are reachable from the named target.",
        bookmarks.iter().map(JJName::raw_name).join(", ")
    );

    let forge = ForgeImpl::new(&repo_config)?;

    let bookmark_graph = BookmarkGraph::from_changes(&jj, &changes, config.revset_options.tracked)?;

    let submission_plan = plan::plan(PlanContext {
        jj: &jj,
        forge: &forge,
        config: &repo_config,
        output,
        bookmark_graph: &bookmark_graph,
        dry_run: config.dry_run,
    })
    .await?;

    if config.show_plan {
        writeln!(output, "Submission plan:\n{submission_plan}")?;
        return Ok(());
    }

    // The stack link needs PRs that existed at planning time, including any
    // that execution leaves unchanged; keep them before the plan is consumed.
    let link_stack = repo_config.forge == ForgeType::GitHub;
    let existing_mrs =
        if link_stack && repo_config.github.link_stack && !config.dry_run && !config.no_hooks {
            submission_plan.existing_mrs.clone()
        } else {
            HashMap::new()
        };

    let result = execute::execute(RootExecuteContext::new(
        &jj,
        &forge,
        &repo_config,
        output,
        config.dry_run,
        submission_plan,
        changes.clone(),
        config.revset_options.tracked,
        config.no_hooks,
    ))
    .await?;

    output.finish();

    if link_stack {
        let outcome = stack_link::link_stacks(
            &repo_config.github,
            &jj,
            &result,
            &existing_mrs,
            config.revset_options.tracked,
            config.dry_run,
            config.no_hooks,
        );
        render_stack_link_outcome(&mut output, &outcome)?;
    }

    writeln!(output, "\n═══════════════════════════════════════")?;
    writeln!(output, "{}", "Summary".bold())?;
    writeln!(output, "═══════════════════════════════════════")?;

    if result.bookmarks_pushed.is_empty() {
        writeln!(output, "No bookmarks pushed")?;
    } else {
        let formatted_bookmarks: Vec<String> = result
            .bookmarks_pushed
            .iter()
            .map(|b| b.magenta().to_string())
            .collect();
        writeln!(output, "Pushed: {}", formatted_bookmarks.join(", "))?;
    }

    if !result.merge_requests.is_empty() {
        writeln!(output, "\n{}\n", format!("{}s:", forge.mr_name()).bold())?;

        let mut table = vec![];

        let mut updates: Vec<_> = result
            .merge_requests
            .iter()
            .sorted_by_key(|mr| mr.mr.iid())
            .collect();

        // If an MR was just created, don't also report that it was updated
        for (index, update) in updates.clone().into_iter().enumerate().rev() {
            if let MRUpdateType::Updated { .. } = update.update_type
                && updates
                    .iter()
                    .find(|u| {
                        u.mr.iid() == update.mr.iid()
                            && matches!(u.update_type, MRUpdateType::Created)
                    })
                    .is_some()
            {
                updates.remove(index);
            }
        }

        updates.dedup_by_key(|u| u.mr.iid());

        for MRUpdate {
            mr,
            bookmark,
            update_type,
            warnings,
        } in updates
        {
            if let Some(warnings) = warnings {
                for warning in warnings {
                    warn!("{}", format!("\nWarning: {warning}").yellow());
                }
            }

            match update_type {
                MRUpdateType::Created => {
                    table.push(vec![
                        bookmark.magenta().cell(),
                        mr.title().wrap(60).cell(),
                        mr.edit_url().dimmed().cell(),
                        "[created]".green().cell(),
                    ]);
                }
                MRUpdateType::Updated { .. } => {
                    table.push(vec![
                        bookmark.magenta().cell(),
                        mr.title().wrap(60).cell(),
                        mr.url().dimmed().cell(),
                        "[updated]".green().cell(),
                    ]);
                }
                MRUpdateType::Unchanged => {
                    table.push(vec![
                        bookmark.magenta().cell(),
                        mr.title().wrap(60).cell(),
                        mr.url().dimmed().cell(),
                        " ".cell(),
                    ]);
                }
            }
        }

        writeln!(
            output,
            "{}",
            table
                .table()
                .border(Border::builder().build())
                .separator(Separator::builder().build())
                .display()
                .expect("Failed to display table")
        )?;
    }

    if !result.errors.is_empty() {
        writeln!(output)?;
        writeln!(output, "✗ {} error(s) occurred:", result.errors.len())?;
        for error in &result.errors {
            writeln!(output, "  • {error}")?;
        }
    }

    if !result.errors.is_empty() {
        return Err(AggregateSnafu {
            errors: result
                .errors
                .into_iter()
                .map::<Box<dyn core::error::Error + 'static>, _>(|e| Box::new(e))
                .collect::<Vec<_>>(),
        }
        .build());
    }

    Ok(())
}

/// Change IDs of the resolved `bookmarks` that the user named literally in
/// `revset`: a bare or double-quoted bookmark name, alone or in a `|` union.
/// Only these bypass the `mine()` filter. A generalized revset such as
/// `bookmarks()` or `mine() & tracked_remote_bookmarks()` names no bookmark,
/// so its targets stay subject to `mine()`.
pub(crate) fn literal_bookmark_targets<'b>(
    revset: &str,
    bookmarks: &'b [BookmarkOrPending<'_>],
) -> impl Iterator<Item = &'b str> {
    let literal_names: HashSet<&str> = revset
        .split('|')
        .map(str::trim)
        .map(|atom| {
            atom.strip_prefix('"')
                .and_then(|quoted| quoted.strip_suffix('"'))
                .unwrap_or(atom)
        })
        .collect();

    bookmarks
        .iter()
        .filter(move |bookmark| bookmark.is_bookmark() && literal_names.contains(bookmark.name()))
        .map(BookmarkOrPending::change_id)
}

fn render_stack_link_outcome(
    output: &mut impl core::fmt::Write,
    outcome: &StackLinkOutcome,
) -> core::fmt::Result {
    match outcome {
        StackLinkOutcome::Linked {
            stacks,
            unlinked,
            warnings,
        } => {
            for stack in stacks {
                let chain = stack
                    .iter()
                    .map(|pr| format!("#{pr}"))
                    .collect::<Vec<_>>()
                    .join(" → ");
                writeln!(output, "{} {chain}", "Stacked:".green().bold())?;
            }
            for note in unlinked {
                writeln!(output, "{}", format!("Note: {note}").yellow())?;
            }
            for warning in warnings {
                writeln!(output, "{}", format!("Warning: {warning}").yellow())?;
            }
        }
        StackLinkOutcome::Failed { warning } => {
            writeln!(output, "{}", format!("Warning: {warning}").yellow())?;
        }
        StackLinkOutcome::Skipped(_) => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::render_stack_link_outcome;
    use crate::submit::stack_link::{SkipReason, StackLinkOutcome};

    #[test]
    fn linked_stack_renders_pull_request_chain() {
        let mut output = String::new();
        let outcome = StackLinkOutcome::Linked {
            stacks: vec![vec![10, 20, 30]],
            unlinked: vec![],
            warnings: vec![],
        };

        render_stack_link_outcome(&mut output, &outcome).unwrap();

        assert!(
            output.contains("#10 → #20 → #30"),
            "chain rendered: {output}"
        );
    }

    #[test]
    fn mixed_stack_link_renders_stacks_notes_and_warnings() {
        let mut output = String::new();
        let outcome = StackLinkOutcome::Linked {
            stacks: vec![vec![1, 2]],
            unlinked: vec!["solo left out".to_owned()],
            warnings: vec!["failed to link stack #3 -> #4".to_owned()],
        };

        render_stack_link_outcome(&mut output, &outcome).unwrap();

        assert!(output.contains("#1 → #2"), "linked stack kept: {output}");
        assert!(
            output.contains("Note: solo left out"),
            "note kept: {output}"
        );
        assert!(
            output.contains("Warning: failed to link stack #3 -> #4"),
            "warning kept: {output}"
        );
    }

    #[test]
    fn failed_stack_link_renders_warning() {
        let mut output = String::new();
        let outcome = StackLinkOutcome::Failed {
            warning: "gh-stack failed".to_owned(),
        };

        render_stack_link_outcome(&mut output, &outcome).unwrap();

        assert!(
            output.contains("Warning: gh-stack failed"),
            "warning rendered: {output}"
        );
    }

    #[test]
    fn missing_binary_skip_renders_nothing() {
        let mut output = String::new();

        render_stack_link_outcome(
            &mut output,
            &StackLinkOutcome::Skipped(SkipReason::MissingBinary),
        )
        .unwrap();

        assert!(output.is_empty());
    }
}

trait WrapText {
    /// Wrap text to the given width by adding newlines at word boundaries.
    fn wrap(&self, max_width: usize) -> Cow<'_, str>;
}

impl<T> WrapText for T
where
    T: AsRef<str>,
{
    /// Wrap text to the given width by adding newlines at word boundaries.
    fn wrap(&self, max_width: usize) -> Cow<'_, str> {
        if self.visual_width() <= max_width {
            return Cow::Borrowed(self.as_ref());
        }

        let mut lines = Vec::new();
        let mut current = String::new();

        for word in self.as_ref().split_word_bounds() {
            if current.visual_width().strict_add(word.visual_width()) > max_width {
                lines.push(current.clone());
                word.trim_start().clone_into(&mut current);
            } else {
                current.push_str(word);
            }
        }

        if !current.is_empty() {
            lines.push(current);
        }

        Cow::Owned(lines.join("\n"))
    }
}
