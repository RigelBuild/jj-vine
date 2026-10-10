#![expect(clippy::module_name_repetitions, reason = "seems fine")]

use core::hash::BuildHasher;
use std::collections::HashSet;

use itertools::Itertools as _;

use crate::{
    bookmark::{BookmarkGraph, BookmarkWithPointers, JJName},
    config::Config,
    error::Result,
    forge::{Forge as _, ForgeImpl},
    jj::{Change, Jujutsu},
    output::Output,
    submit::plan::SubmissionPlan,
};

pub mod execute;
pub mod plan;
pub mod stack_link;

/// Find the changes that matter for a submission starting from `targets`:
/// bookmarked changes reachable from the targets that are not already in the
/// trunk ancestry. Only `explicit_targets` (bookmarks the user named
/// literally) are included regardless of their author; every other target and
/// every ancestry-walked bookmark is limited to the current user. A change
/// included only through the bypass keeps just its explicitly named bookmarks,
/// so another bookmark on the same change is not submitted with it.
pub fn find_changes_to_submit(
    jj: &Jujutsu,
    targets: impl IntoIterator<Item = impl JJName>,
    explicit_targets: impl IntoIterator<Item = impl JJName>,
    change_ids_pending_bookmarks: &HashSet<String, impl BuildHasher>,
) -> Result<Vec<Change>> {
    let explicit_names: HashSet<String> =
        explicit_targets.into_iter().map(|t| t.raw_name()).collect();
    let target_atoms: Vec<String> = targets.into_iter().map(|t| t.name_for_jj()).collect();

    let explicit = if explicit_names.is_empty() {
        "none()".to_owned()
    } else {
        explicit_names
            .iter()
            .map(|name| format!("bookmarks(exact:{})", revset_string_literal(name)))
            .join(" | ")
    };
    let target_set = if target_atoms.is_empty() {
        "none()".to_owned()
    } else {
        target_atoms.iter().join(" | ")
    };
    let pending = if change_ids_pending_bookmarks.is_empty() {
        "none()".to_owned()
    } else {
        change_ids_pending_bookmarks.iter().join(" | ")
    };

    // Only a target that is itself submitted seeds the ancestry walk. A target
    // dropped by `mine()` must not pull in the user's bookmarks below it.
    let ancestry = format!("::(({target_set}) & (mine() | ({explicit}) | ({pending})))");

    let mut changes = jj.log_with_pending_bookmarks(
        format!(
            "(({explicit}) | (({ancestry}) & mine() & bookmarks()) | ({pending})) ~ (::trunk())"
        ),
        change_ids_pending_bookmarks,
    )?;

    if !explicit_names.is_empty() {
        let bypassed: HashSet<String> = jj
            .log(format!("({explicit}) ~ mine()"))?
            .into_iter()
            .map(|change| change.change_id)
            .collect();

        for change in &mut changes {
            if bypassed.contains(&change.change_id) {
                change
                    .bookmarks
                    .retain(|bookmark| explicit_names.contains(bookmark.name()));
            }
        }
    }

    Ok(changes)
}

/// Quote `value` as a jj revset string literal, escaping per jj's grammar.
fn revset_string_literal(value: &str) -> String {
    let mut literal = String::with_capacity(value.len().saturating_add(2));
    literal.push('"');
    for c in value.chars() {
        match c {
            '"' => literal.push_str("\\\""),
            '\\' => literal.push_str("\\\\"),
            '\t' => literal.push_str("\\t"),
            '\r' => literal.push_str("\\r"),
            '\n' => literal.push_str("\\n"),
            '\0' => literal.push_str("\\0"),
            '\x1b' => literal.push_str("\\e"),
            c => literal.push(c),
        }
    }
    literal.push('"');
    literal
}

/// Since we can't make a PR/MR between two fork branches, on the *target*
/// repository, we can only really make all branches
/// on the upstream. Returns the base branch to use for PRs/MRs.
#[must_use]
pub fn mr_base_branch(
    forge: &ForgeImpl,
    bookmark: &BookmarkWithPointers,
    default_branch: &str,
) -> String {
    if forge.is_fork() {
        default_branch.to_owned()
    } else {
        bookmark.parent_name(default_branch)
    }
}

#[derive(Clone)]
pub struct PlanContext<'a> {
    pub jj: &'a Jujutsu,
    pub forge: &'a ForgeImpl,
    pub config: &'a Config,
    pub output: &'a dyn Output,
    pub bookmark_graph: &'a BookmarkGraph<'a>,
    pub dry_run: bool,
}

#[derive(Clone)]
pub struct ExecuteContext<'a> {
    pub jj: &'a Jujutsu,
    pub forge: &'a ForgeImpl,
    pub config: &'a Config,
    pub output: &'a dyn Output,
    pub bookmark_graph: &'a BookmarkGraph<'a>,
    pub dry_run: bool,
    pub no_hooks: bool,

    pub plan: &'a SubmissionPlan,
}

impl<'a> ExecuteContext<'a> {
    #[must_use]
    pub fn new(ctx: &'a RootExecuteContext<'a>, bookmark_graph: &'a BookmarkGraph<'a>) -> Self {
        Self {
            jj: ctx.jj,
            forge: ctx.forge,
            config: ctx.config,
            output: ctx.output,
            bookmark_graph,
            dry_run: ctx.dry_run,
            no_hooks: ctx.no_hooks,
            plan: &ctx.plan,
        }
    }
}

pub struct RootExecuteContext<'a> {
    pub jj: &'a Jujutsu,
    pub forge: &'a ForgeImpl,
    pub config: &'a Config,
    pub output: &'a dyn Output,
    pub dry_run: bool,
    pub no_hooks: bool,

    pub plan: SubmissionPlan,
    pub changes: Vec<Change>,
    pub skip_untracked_local_bookmarks: bool,
}

impl<'a> RootExecuteContext<'a> {
    #[expect(clippy::too_many_arguments, reason = "really need them all")]
    pub fn new(
        jj: &'a Jujutsu,
        forge: &'a ForgeImpl,
        config: &'a Config,
        output: &'a dyn Output,
        dry_run: bool,
        plan: SubmissionPlan,
        changes: Vec<Change>,
        skip_untracked_local_bookmarks: bool,
        no_hooks: bool,
    ) -> Self {
        Self {
            jj,
            forge,
            config,
            output,
            dry_run,
            no_hooks,
            plan,
            changes,
            skip_untracked_local_bookmarks,
        }
    }
}
