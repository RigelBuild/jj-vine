pub mod create_mr;
pub mod push;
pub mod push_create;
pub mod sync_dependent_merge_requests;
pub mod update_mr_base;
pub mod update_mr_title_description;

use std::collections::{HashMap, HashSet};

use bon::bon;
use enum_dispatch::enum_dispatch;
use futures::{StreamExt as _, stream::FuturesUnordered};
use itertools::{Either, Itertools as _};
use snafu::whatever;
use tracing::debug;

use crate::{
    bookmark::{
        BookmarkGraph,
        BookmarkOrPending,
        BookmarkRef,
        BookmarkWithPointers,
        change_id_to_temp_bookmark_name,
    },
    error::{ClonableError, Error, Result},
    forge::AnyForgeMergeRequest,
    jj::{BookmarkInfo, Change},
    submit::{
        ExecuteContext,
        RootExecuteContext,
        execute::{
            create_mr::CreateMRAction,
            push::PushAction,
            push_create::PushCreateAction,
            sync_dependent_merge_requests::SyncDependentMergeRequestsAction,
            update_mr_base::UpdateMRBaseAction,
            update_mr_title_description::UpdateMRTitleDescriptionAction,
        },
    },
};

/// Action to perform during execution
#[derive(Debug, Clone, PartialEq)]
#[enum_dispatch(ExecuteAction, ActionInfo)]
pub enum Action {
    Push(PushAction),
    PushCreate(PushCreateAction),
    CreateMR(CreateMRAction),
    UpdateMRBase(UpdateMRBaseAction),
    UpdateMRTitleDescription(UpdateMRTitleDescriptionAction),
    SyncDependentMergeRequests(SyncDependentMergeRequestsAction),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BookmarkNameOrPendingChangeId {
    Bookmark(String),
    PendingChangeId(String),
}

impl BookmarkNameOrPendingChangeId {
    #[must_use]
    pub fn new_from_bookmark(bookmark: &BookmarkOrPending<'_>) -> Self {
        match bookmark {
            BookmarkOrPending::Bookmark(b) => Self::Bookmark(b.name().to_owned()),
            BookmarkOrPending::Pending { change, .. } => {
                Self::PendingChangeId(change.change_id.clone())
            }
        }
    }

    #[must_use]
    pub fn new_from_pointer(pointer: &BookmarkWithPointers<'_>) -> Self {
        Self::new_from_bookmark(&pointer.bookmark)
    }
}

impl core::fmt::Display for BookmarkNameOrPendingChangeId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Bookmark(name) => write!(f, "{name}"),
            Self::PendingChangeId(change_id) => {
                write!(f, "{}", change_id_to_temp_bookmark_name(change_id))
            }
        }
    }
}

/// Result of executing a submission plan.
#[derive(Debug)]
pub struct SubmissionResult {
    /// All MRs (created, updated, and unchanged).
    pub merge_requests: Vec<MRUpdate>,

    /// Any errors that occurred (non-fatal).
    pub errors: Vec<ClonableError>,

    /// Bookmarks that were successfully pushed.
    pub bookmarks_pushed: Vec<String>,

    /// Changes after execution has assigned names to pending bookmarks.
    pub changes: Vec<Change>,
}

#[derive(Debug, Clone)]
pub struct MRUpdate {
    pub mr: AnyForgeMergeRequest,
    pub bookmark: String,
    pub update_type: MRUpdateType,
    pub warnings: Option<Vec<String>>,
}

#[derive(Debug, Clone)]
pub enum ActionResultData {
    Pushed {
        bookmarks: Vec<String>,
        created_bookmarks: HashMap<String, String>,
        pushed: bool,
    },
    MRCreated(MRUpdate),
    MRUpdated(MRUpdate),
    DryRun,
    /// Skipped because pushing is disabled or a merge request it needs was
    /// not created.
    Skipped,
}

#[derive(Debug, Clone)]
pub struct ActionResult {
    pub id: String,
    pub data: Result<ActionResultData, ClonableError>,
}

pub struct ExecuteActionContext<'a> {
    pub execute: ExecuteContext<'a>,
    pub current_results: Vec<ActionResult>,
}

impl ExecuteActionContext<'_> {
    /// Gets all MRs at the current state of execution by overlaying creations
    /// and updates on top of MRs that existed at planning time.
    #[must_use]
    pub fn all_mrs(&self) -> HashMap<String, AnyForgeMergeRequest> {
        let mut all_mrs = self.execute.plan.existing_mrs.clone();

        for result in &self.current_results {
            if let Ok(ActionResultData::MRCreated(update) | ActionResultData::MRUpdated(update)) =
                &result.data
            {
                all_mrs.insert(update.bookmark.clone(), update.mr.clone());
            }
        }

        all_mrs
    }
    #[must_use]
    pub fn find_bookmark_name(&self, bookmark: &BookmarkNameOrPendingChangeId) -> Option<String> {
        match bookmark {
            BookmarkNameOrPendingChangeId::Bookmark(name) => Some(name.clone()),
            BookmarkNameOrPendingChangeId::PendingChangeId(change_id) => {
                for result in &self.current_results {
                    if let Ok(ActionResultData::Pushed {
                        created_bookmarks, ..
                    }) = &result.data
                        && let Some(name) = created_bookmarks.get(change_id)
                    {
                        return Some(name.clone());
                    }
                }
                None
            }
        }
    }

    pub fn find_bookmark_name_required(
        &self,
        bookmark: &BookmarkNameOrPendingChangeId,
    ) -> Result<String> {
        match self.find_bookmark_name(bookmark) {
            Some(name) => Ok(name),
            None => {
                whatever!("Could not find a created bookmark for change {}", bookmark);
            }
        }
    }
}

#[enum_dispatch]
pub trait ExecuteAction {
    async fn execute(&self, ctx: ExecuteActionContext<'_>) -> Result<ActionResultData>;
}

#[enum_dispatch]
pub trait ActionInfo {
    /// Gets a unique ID for this action that other actions can potentially
    /// refer to.
    fn id(&self) -> String;

    /// The text to display for an entire group of this action type.
    fn group_text(&self) -> String;

    /// The text to display for this action.
    fn text(&self) -> String;

    /// The text to display for a substep that is this action.
    fn substep_text(&self) -> String;

    /// The text to display for this action when showing the plan.
    fn plan_text(&self) -> String;

    /// Gets any dependencies that this action has on other actions.
    fn dependencies(&self) -> Vec<String> {
        vec![]
    }
}

/// Type of MR update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MRUpdateType {
    /// MR was unchanged.
    Unchanged,

    /// MR was created.
    Created,

    /// Merge requested was updated in some way.
    Updated {
        old_target: Option<String>,
        new_target: Option<String>,

        old_title: Option<String>,
        new_title: Option<String>,

        old_description: Option<String>,
        new_description: Option<String>,

        synced_dependent_merge_requests: bool,
    },
}

#[bon]
impl MRUpdateType {
    #[builder]
    #[expect(clippy::single_call_fn, reason = "important")]
    pub fn new_updated(
        old_target: Option<String>,
        new_target: Option<String>,
        old_title: Option<String>,
        new_title: Option<String>,
        old_description: Option<String>,
        new_description: Option<String>,
        synced_dependent_merge_requests: Option<bool>,
    ) -> Self {
        Self::Updated {
            old_target,
            new_target,
            old_title,
            new_title,
            old_description,
            new_description,
            synced_dependent_merge_requests: synced_dependent_merge_requests.unwrap_or(false),
        }
    }
}

/// Execute a submission plan.
///
/// # Panics
///
/// Panics if many reasons.
#[expect(clippy::too_many_lines, reason = "important")]
pub async fn execute(mut ctx: RootExecuteContext<'_>) -> Result<SubmissionResult> {
    let mut merge_requests = Vec::new();
    let mut errors = Vec::new();
    let mut bookmarks_pushed = Vec::new();
    let mut absent_mr_bookmarks = HashSet::new();
    let mut current_results: Vec<ActionResult> = Vec::new();

    let mut bookmark_graph =
        BookmarkGraph::from_changes(ctx.jj, &ctx.changes, ctx.skip_untracked_local_bookmarks)?;
    let unpushed_targets = unpushed_push_targets(&ctx);

    ctx.output.log_current("Preparing submission");

    for batch in &ctx.plan.actions {
        ctx.output.log_current(&batch.first().unwrap().group_text());

        let handles = FuturesUnordered::new();

        for action in batch {
            let (_ok_deps, err_deps): (HashMap<_, _>, Vec<_>) = action
                .dependencies()
                .iter()
                .map(|id| {
                    current_results
                        .iter()
                        .find(|result| result.id == *id)
                        .unwrap_or_else(|| panic!("Dependency {id} not found"))
                })
                .partition_map(|dep| match &dep.data {
                    Ok(data) => Either::Left((&dep.id, data)),
                    Err(error) => Either::Right((&dep.id, error)),
                });

            if !err_deps.is_empty() {
                debug!(
                    "Skipping action {} because dependencies failed: {}",
                    action.id(),
                    err_deps.iter().map(|(id, _)| id).join(", ")
                );
                current_results.push(ActionResult {
                    id: action.id(),
                    data: Err(Error::new(format!(
                        "Dependencies failed: {}",
                        err_deps.iter().map(|(id, _)| id).join(", ")
                    ))
                    .to_clonable_error()),
                });
                continue;
            }

            if targets_unpushed_bookmark(action, &unpushed_targets, &bookmark_graph)
                || action_needs_absent_mr(action, &absent_mr_bookmarks, &bookmark_graph)
            {
                debug!(
                    "Skipping action {} because pushing is disabled",
                    action.id()
                );
                ctx.output.log_message(&format!(
                    "{} because pushing is disabled: {}",
                    if ctx.dry_run {
                        "Would skip"
                    } else {
                        "Skipping"
                    },
                    action.plan_text()
                ));
                // Only a skipped creation leaves an MR missing. A skipped
                // update leaves the existing MR in place.
                if let Action::CreateMR(create_mr) = action {
                    absent_mr_bookmarks.insert(create_mr.bookmark.to_string());
                }
                current_results.push(ActionResult {
                    id: action.id(),
                    data: Ok(ActionResultData::Skipped),
                });
                continue;
            }

            let action_id = action.id();

            let action_ctx = ExecuteActionContext {
                execute: ExecuteContext::new(&ctx, &bookmark_graph),
                current_results: current_results.clone(),
            };

            handles.push(async move {
                let output = action_ctx.execute.output;

                let _substep = output.start_substep(&action.substep_text());
                let result = action.execute(action_ctx).await;

                (action_id, result)
            });
        }

        let results = handles.collect::<Vec<_>>().await;

        for (action_id, result) in results {
            current_results.push(ActionResult {
                id: action_id,
                data: result.as_ref().map_err(Error::to_clonable_error).cloned(),
            });

            if let Ok(ActionResultData::Pushed {
                created_bookmarks, ..
            }) = &result
            {
                #[expect(clippy::iter_over_hash_type, reason = "don't need ordering here")]
                for (change_id, name) in created_bookmarks {
                    let change = ctx
                        .changes
                        .iter_mut()
                        .find(|c| c.change_id == *change_id)
                        .unwrap_or_else(|| panic!("Could not find change {change_id} in changes"));

                    change.solidify_bookmark(name);
                }

                // Rebuild the graph using the new changes
                bookmark_graph = BookmarkGraph::from_changes(
                    ctx.jj,
                    &ctx.changes,
                    ctx.skip_untracked_local_bookmarks,
                )?;
            }
        }
    }

    for result in current_results {
        match result.data {
            Ok(ActionResultData::Pushed {
                bookmarks, pushed, ..
            }) => {
                if pushed {
                    bookmarks_pushed.extend(bookmarks);
                }
            }
            Ok(ActionResultData::MRCreated(mr_update) | ActionResultData::MRUpdated(mr_update)) => {
                merge_requests.push(mr_update);
            }
            Ok(ActionResultData::DryRun | ActionResultData::Skipped) => {}
            Err(error) => {
                errors.push(error);
            }
        }
    }

    Ok(SubmissionResult {
        merge_requests,
        errors,
        bookmarks_pushed,
        changes: ctx.changes,
    })
}

/// Whether a merge request action needs an MR whose creation was skipped
/// because pushing is disabled. Actions run in topological order, so checking
/// direct parents also covers MRs missing further down the stack.
fn action_needs_absent_mr(
    action: &Action,
    absent_mr_bookmarks: &HashSet<String>,
    bookmark_graph: &BookmarkGraph<'_>,
) -> bool {
    if absent_mr_bookmarks.is_empty() {
        return false;
    }

    let is_absent = |name: &str| absent_mr_bookmarks.contains(name);

    match action {
        Action::Push(_) | Action::PushCreate(_) => false,
        Action::CreateMR(create_mr) => is_absent(&create_mr.target_branch),
        Action::UpdateMRBase(update_mr_base) => is_absent(&update_mr_base.new_target_branch),
        // The stack description lists every MR in the component.
        Action::UpdateMRTitleDescription(update) => bookmark_graph
            .component_containing(&update.bookmark.to_string())
            .is_some_and(|component| {
                component
                    .all_bookmarks()
                    .iter()
                    .any(|bookmark| is_absent(bookmark.name()))
            }),
        // Dependency sync reads its own MR and the MRs of its direct parents.
        Action::SyncDependentMergeRequests(sync) => {
            let name = sync.bookmark.to_string();

            is_absent(&name)
                || bookmark_graph
                    .find_bookmark_in_components(&name)
                    .is_some_and(|bookmark| {
                        bookmark.parents.iter().any(|parent| match parent {
                            BookmarkRef::Bookmark(parent) => is_absent(parent.name()),
                            BookmarkRef::Trunk => false,
                        })
                    })
        }
    }
}

/// Planned push targets whose head on the push remote will not match the local
/// head because pushing is disabled. Empty unless pushing is disabled.
///
/// A bookmark already tracked and in sync on the push action's own remote
/// does not depend on the push, so merge request actions for it still run.
/// Sync with any other remote does not count. When the push remote's state
/// cannot be read, every bookmark counts as unpushed.
fn unpushed_push_targets(ctx: &RootExecuteContext<'_>) -> Vec<BookmarkNameOrPendingChangeId> {
    let mut targets = Vec::new();

    if ctx.config.push.resolve_argv(ctx.no_hooks).is_some() {
        return targets;
    }

    for action in ctx.plan.actions.iter().flatten() {
        match action {
            Action::Push(push) => {
                let synced = ctx
                    .jj
                    .bookmarks_synced_with_remote(
                        push.bookmarks.iter().map(String::as_str),
                        &push.remote,
                    )
                    .unwrap_or_else(|error| {
                        debug!(
                            "Could not read bookmark state on remote {}; treating all as unpushed: {error}",
                            push.remote
                        );
                        HashSet::new()
                    });

                targets.extend(
                    push.bookmarks
                        .iter()
                        .filter(|name| !synced.contains(*name))
                        .cloned()
                        .map(BookmarkNameOrPendingChangeId::Bookmark),
                );
            }
            Action::PushCreate(push_create) => targets.extend(
                push_create
                    .change_ids
                    .iter()
                    .cloned()
                    .map(BookmarkNameOrPendingChangeId::PendingChangeId),
            ),
            Action::CreateMR(_)
            | Action::UpdateMRBase(_)
            | Action::UpdateMRTitleDescription(_)
            | Action::SyncDependentMergeRequests(_) => {}
        }
    }

    targets
}

/// Whether a merge request action reads or writes a branch whose remote head
/// is stale or missing. Push actions handle disabled pushing themselves.
fn targets_unpushed_bookmark(
    action: &Action,
    unpushed_targets: &[BookmarkNameOrPendingChangeId],
    bookmark_graph: &BookmarkGraph<'_>,
) -> bool {
    if unpushed_targets.is_empty() {
        return false;
    }

    // A pending target is named by its temporary bookmark name until pushed;
    // a child of a pending parent targets that name.
    let is_unpushed_name = |name: &str| {
        unpushed_targets.iter().any(|target| match target {
            BookmarkNameOrPendingChangeId::Bookmark(unpushed) => unpushed == name,
            BookmarkNameOrPendingChangeId::PendingChangeId(change_id) => {
                change_id_to_temp_bookmark_name(change_id) == name
            }
        })
    };

    match action {
        Action::Push(_) | Action::PushCreate(_) => false,
        Action::CreateMR(create_mr) => {
            unpushed_targets.contains(&create_mr.bookmark)
                || is_unpushed_name(&create_mr.target_branch)
        }
        Action::UpdateMRBase(update_mr_base) => {
            is_unpushed_name(&update_mr_base.bookmark)
                || is_unpushed_name(&update_mr_base.new_target_branch)
        }
        // The stack description lists every MR in the component, so one
        // unpushed member makes it stale or references a missing MR.
        Action::UpdateMRTitleDescription(update) => {
            let name = update.bookmark.to_string();

            is_unpushed_name(&name)
                || bookmark_graph
                    .component_containing(&name)
                    .is_some_and(|component| {
                        component
                            .all_bookmarks()
                            .iter()
                            .any(|bookmark| is_unpushed_name(bookmark.name()))
                    })
        }
        Action::SyncDependentMergeRequests(sync) => {
            let BookmarkNameOrPendingChangeId::Bookmark(name) = &sync.bookmark else {
                return unpushed_targets.contains(&sync.bookmark);
            };

            is_unpushed_name(name)
                || bookmark_graph
                    .find_bookmark_in_components(name)
                    .is_some_and(|bookmark| {
                        bookmark.parents.iter().any(|parent| match parent {
                            BookmarkRef::Bookmark(parent) => is_unpushed_name(parent.name()),
                            BookmarkRef::Trunk => false,
                        })
                    })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submission_result_changes_carry_solidified_bookmark_names() {
        let change_id = "abcd1234ef";
        let mut change = Change::mock_from_change_id(change_id);
        change.pending_bookmark = true;
        assert!(change.bookmarks.is_empty());

        let pending_display = change_id_to_temp_bookmark_name(change_id);
        assert_eq!(pending_display, "(new bookmark for abcd1234)");

        let solidified = "feature-real-name";
        change.solidify_bookmark(solidified);
        let result = SubmissionResult {
            merge_requests: vec![],
            errors: vec![],
            bookmarks_pushed: vec![],
            changes: vec![change],
        };

        let names: Vec<&str> = result
            .changes
            .iter()
            .flat_map(|change| change.bookmarks.iter().map(BookmarkInfo::name))
            .collect();
        assert_eq!(names, vec![solidified]);
        assert!(!result.changes[0].pending_bookmark);
    }
}
