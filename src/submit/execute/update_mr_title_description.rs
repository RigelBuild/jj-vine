use bon::Builder;
use owo_colors::OwoColorize as _;
use snafu::whatever;
use tracing::error;

use crate::{
    description::{
        FormatMergeRequest as _,
        generate_stack_description,
        insert_stack_into_description,
    },
    error::{Error, Result},
    forge::{Forge as _, UpdateMergeRequestInfoOptions},
    submit::execute::{
        ActionInfo,
        ActionResultData,
        BookmarkNameOrPendingChangeId,
        ExecuteAction,
        ExecuteActionContext,
        MRUpdate,
        MRUpdateType,
    },
};

/// Update MR description (after all MRs created).
#[derive(Debug, Clone, PartialEq, Eq, Builder)]
pub struct UpdateMRTitleDescriptionAction {
    /// The new title for the MR. If None, the title will not be updated.
    pub title: Option<String>,

    /// The new user-content description for the MR. If None, the description
    /// will not be updated. This does not affect whether the stack is shown
    /// or regenerated, it is only for syncing the description automatically.
    pub description: Option<String>,

    pub generate_stack_in_description: bool,

    pub bookmark: BookmarkNameOrPendingChangeId,

    pub dependencies: Option<Vec<String>>,
}

impl ActionInfo for UpdateMRTitleDescriptionAction {
    fn id(&self) -> String {
        format!("update_mr_title_description:{}", self.bookmark)
    }

    fn group_text(&self) -> String {
        "Updating MR descriptions".to_owned()
    }

    fn text(&self) -> String {
        format!("Updating MR {} description", self.bookmark.magenta())
    }

    fn substep_text(&self) -> String {
        self.bookmark.magenta().to_string()
    }

    fn plan_text(&self) -> String {
        match (
            &self.title,
            self.generate_stack_in_description,
            &self.description,
        ) {
            (Some(title), true, Some(description)) => format!(
                "Update title of MR for {} to \"{}\" and update description ({} lines) & stack",
                self.bookmark.magenta(),
                title.bold(),
                description.lines().count()
            ),
            (Some(title), true, None) => format!(
                "Update title of MR for {} to \"{}\" and regenerate stack in description",
                self.bookmark.magenta(),
                title.bold()
            ),
            (Some(title), false, None) => format!(
                "Update title of MR for {} to \"{}\"",
                self.bookmark.magenta(),
                title.bold()
            ),
            (Some(title), false, Some(description)) => format!(
                "Update title of MR for {} to \"{}\" and update description ({} lines)",
                self.bookmark.magenta(),
                title.bold(),
                description.lines().count()
            ),
            (None, true, None) => format!(
                "Regenerate stack in description of MR for {}",
                self.bookmark.magenta()
            ),
            (None, true, Some(description)) => format!(
                "Update description ({} lines) & stack of MR for {}",
                description.lines().count(),
                self.bookmark.magenta()
            ),
            (None, false, Some(description)) => {
                format!(
                    "Update description of MR for {} ({} lines)",
                    self.bookmark.magenta(),
                    description.lines().count()
                )
            }
            (None, false, None) => format!(
                "ERROR: Neither title nor generate_stack_in_description nor description is set for {}",
                self.id(),
            ),
        }
    }

    fn dependencies(&self) -> Vec<String> {
        self.dependencies.clone().unwrap_or_default()
    }
}

impl ExecuteAction for UpdateMRTitleDescriptionAction {
    #[expect(clippy::too_many_lines, reason = "it's fine")]
    async fn execute(&self, ctx: ExecuteActionContext<'_>) -> Result<ActionResultData> {
        let bookmark = ctx.find_bookmark_name_required(&self.bookmark)?;

        let all_mrs = ctx.all_mrs();

        let Some(current_mr) = all_mrs.get(bookmark.as_str()) else {
            if ctx.execute.dry_run {
                return Ok(ActionResultData::DryRun);
            }

            whatever!("No MR found for {}", bookmark.magenta());
        };

        let default_branch = ctx.execute.jj.default_branch()?;

        let Some(stack) = ctx.execute.bookmark_graph.component_containing(&bookmark) else {
            whatever!("Bookmark not found in component: {}", self.bookmark);
        };

        let stack_description = generate_stack_description(
            &bookmark,
            stack,
            &all_mrs,
            &ctx.execute.config.description,
            default_branch,
            ctx.execute.forge,
        );

        let description_user_part = if let Some(description) = &self.description {
            description // Stack part will be inserted according to configured placement.
        } else {
            current_mr.description()
        };

        let new_description = insert_stack_into_description(
            &stack_description,
            description_user_part,
            ctx.execute.config.description.placement,
        );

        let description_unchanged = current_mr.description() == new_description;

        if description_unchanged && self.title.is_none() {
            return Ok(ActionResultData::MRUpdated(MRUpdate {
                mr: current_mr.clone(),
                bookmark: bookmark.clone(),
                update_type: MRUpdateType::Unchanged,
                warnings: None,
            }));
        }

        if ctx.execute.dry_run {
            if let Some(title) = self.title.as_ref() {
                ctx.execute.output.log_message(&format!(
                    "Would {} the title of {} {} to \"{}\"",
                    "update".yellow(),
                    ctx.execute.forge.mr_name(),
                    ctx.execute
                        .forge
                        .format_merge_request_id(current_mr.iid())
                        .cyan(),
                    title,
                ));
            }
            if !description_unchanged {
                ctx.execute.output.log_message(&format!(
                    "Would {} the description of {} {}",
                    "update".yellow(),
                    ctx.execute.forge.mr_name(),
                    ctx.execute
                        .forge
                        .format_merge_request_id(current_mr.iid())
                        .cyan(),
                ));
            }
            return Ok(ActionResultData::DryRun);
        }

        match ctx
            .execute
            .forge
            .update_merge_request_info(
                current_mr.iid(),
                UpdateMergeRequestInfoOptions::builder()
                    .description(new_description.clone())
                    .maybe_title(self.title.clone())
                    .current_is_draft(current_mr.is_draft())
                    .current_title(current_mr.title().to_owned())
                    .build(),
            )
            .await
        {
            Ok(updated_mr) => {
                ctx.execute.output.log_completed(&format!(
                    "Updated MR {} description",
                    format!("!{}", updated_mr.iid()).cyan()
                ));

                Ok(ActionResultData::MRUpdated(MRUpdate {
                    mr: updated_mr,
                    bookmark: bookmark.clone(),
                    update_type: MRUpdateType::new_updated()
                        .old_description(current_mr.description().to_owned())
                        .maybe_new_description(
                            description_unchanged.then(|| new_description.clone()),
                        )
                        .old_title(current_mr.title().to_owned())
                        .maybe_new_title(self.title.clone())
                        .call(),
                    warnings: None,
                }))
            }
            Err(e) => {
                let error_msg = format!("Failed to update MR description for {bookmark}: {e}");
                ctx.execute.output.log_message(&error_msg);
                error!("{}", error_msg);
                Err(Error::new(error_msg))
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use std::{borrow::Cow, collections::HashMap};

    use super::*;
    use crate::{
        bookmark::BookmarkGraph,
        config::{
            Config,
            DescriptionConfig,
            DescriptionDiagramConfig,
            DescriptionDiagramFormat,
            ForgeType,
            StackPlacement,
        },
        description::{END_MARKER, START_MARKER},
        forge::{
            AnyForgeMergeRequest,
            ForgeImpl,
            test::{MergeRequest, TestForge},
        },
        output::FlatOutput,
        submit::{ExecuteContext, plan::SubmissionPlan},
        tests::TestRepo,
    };

    #[tokio::test]
    async fn execute_places_refreshed_stack_above_existing_user_text() -> Result<()> {
        let repo = TestRepo::with_main();
        repo.set_config(r#"revset-aliases."trunk()""#, "main");
        let bookmark = repo.bookmark_name("placement");
        repo.create_change_and_bookmark(&bookmark);
        let changes = repo.jj.log("all()")?;
        let bookmark_graph = BookmarkGraph::from_changes(&repo.jj, &changes, false)?;

        let existing_description =
            format!("{START_MARKER}\nOutdated stack content\n{END_MARKER}\n\nUser notes");
        let existing_mr = MergeRequest::builder()
            .id("1".to_owned())
            .title("Placement test".to_owned())
            .description(existing_description)
            .source_branch(bookmark.clone())
            .target_branch("main".to_owned())
            .build();
        let test_forge = TestForge::builder()
            .merge_requests(HashMap::from([("1".to_owned(), existing_mr.clone())]))
            .build();
        let forge = ForgeImpl::Test(test_forge);
        let config = Config::builder()
            .forge(ForgeType::Forgejo)
            .description(DescriptionConfig {
                placement: StackPlacement::Top,
                diagram: DescriptionDiagramConfig {
                    single: DescriptionDiagramFormat::Linear,
                    ..DescriptionDiagramConfig::default()
                },
                ..DescriptionConfig::default()
            })
            .build();
        let output = FlatOutput::default();
        let plan = SubmissionPlan {
            actions: Vec::new(),
            existing_mrs: HashMap::from([(
                bookmark.clone(),
                AnyForgeMergeRequest::new(existing_mr),
            )]),
        };
        let action = UpdateMRTitleDescriptionAction::builder()
            .bookmark(BookmarkNameOrPendingChangeId::Bookmark(bookmark))
            .generate_stack_in_description(true)
            .build();

        let result = action
            .execute(ExecuteActionContext {
                execute: ExecuteContext {
                    jj: &repo.jj,
                    forge: &forge,
                    config: &config,
                    output: &output,
                    bookmark_graph: &bookmark_graph,
                    dry_run: false,
                    plan: &plan,
                },
                current_results: Vec::new(),
            })
            .await?;
        assert!(
            matches!(result, ActionResultData::MRUpdated(_)),
            "expected action execution to update the merge request",
        );

        let ForgeImpl::Test(test_forge) = &forge else {
            return Err(crate::error::Error::new("expected TestForge"));
        };
        let updated_mr = test_forge.get_merge_request(Cow::Borrowed("1")).await?;
        let expected_stack = concat!(
            "This MR is part of a stack containing 1 MR:\n\n",
            "1. `main`\n",
            "2. **\"Placement test\" (this MR)**",
        );
        let expected_description =
            format!("{START_MARKER}\n{expected_stack}\n{END_MARKER}\n\nUser notes");

        assert_eq!(
            updated_mr.description.as_deref(),
            Some(expected_description.as_str()),
            "updated TestForge MR should place regenerated markers above user text",
        );

        Ok(())
    }
}
