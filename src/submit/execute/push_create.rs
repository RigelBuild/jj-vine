use std::collections::HashMap;

use bon::Builder;
use itertools::Itertools as _;
use owo_colors::OwoColorize as _;
use tracing::{debug, error};

use crate::{
    bookmark::{Bookmark, change_id_to_temp_bookmark_name},
    config::push_description,
    error::{Error, Result},
    submit::execute::{ActionInfo, ActionResultData, ExecuteAction, ExecuteActionContext},
};

/// Push changes to a remote using -c.
#[derive(Debug, Clone, PartialEq, Eq, Builder)]
pub struct PushCreateAction {
    pub change_ids: Vec<String>,

    pub remote: String,
}

impl ActionInfo for PushCreateAction {
    fn id(&self) -> String {
        format!("push_create:{}", self.change_ids.join(","))
    }

    fn group_text(&self) -> String {
        "Creating and pushing bookmarks".to_owned()
    }

    #[expect(clippy::string_slice, reason = "change_ids are ASCII")]
    fn text(&self) -> String {
        format!(
            "Creating and pushing {}",
            self.change_ids
                .iter()
                .map(|c| (&c[..8]).magenta().to_string())
                .join(", ")
        )
    }

    #[expect(clippy::string_slice, reason = "change_ids are ASCII")]
    fn substep_text(&self) -> String {
        self.change_ids
            .iter()
            .map(|c| (&c[..8]).magenta().to_string())
            .join(", ")
    }

    #[expect(clippy::string_slice, reason = "change_ids are ASCII")]
    fn plan_text(&self) -> String {
        format!(
            "Create and push bookmarks to remote {} for changes: {}",
            self.remote.cyan(),
            self.change_ids
                .iter()
                .map(|c| (&c[..8]).magenta().to_string())
                .join(", "),
        )
    }
}

impl ExecuteAction for PushCreateAction {
    async fn execute(&self, ctx: ExecuteActionContext<'_>) -> Result<ActionResultData> {
        #[expect(clippy::string_slice, reason = "change_ids are ASCII")]
        let change_ids_string = self
            .change_ids
            .iter()
            .map(|change_id| (&change_id[..8]).magenta().to_string())
            .join(", ");
        let push_argv = ctx.execute.config.push.resolve_argv(ctx.execute.no_hooks);

        if ctx.execute.dry_run {
            let push_description = push_description(push_argv.as_deref());
            ctx.execute.output.log_message(&format!(
                "Would {} and push to remote {} {push_description} for changes: {change_ids_string}",
                "create bookmarks".green(),
                self.remote.cyan()
            ));

            if push_argv.is_some() {
                let bookmarks = self
                    .change_ids
                    .iter()
                    .map(|change_id| change_id_to_temp_bookmark_name(change_id))
                    .collect::<Vec<_>>();
                let created_bookmarks = self
                    .change_ids
                    .iter()
                    .map(|change_id| {
                        (
                            change_id.clone(),
                            change_id_to_temp_bookmark_name(change_id),
                        )
                    })
                    .collect();

                Ok(ActionResultData::Pushed {
                    bookmarks,
                    created_bookmarks,
                    pushed: push_argv.is_some(),
                })
            } else {
                Ok(ActionResultData::Pushed {
                    bookmarks: Vec::new(),
                    created_bookmarks: HashMap::new(),
                    pushed: false,
                })
            }
        } else {
            let Some(push_argv) = push_argv else {
                debug!("Pushing disabled; skipping create+push {change_ids_string}");
                return Ok(ActionResultData::Pushed {
                    bookmarks: Vec::new(),
                    created_bookmarks: HashMap::new(),
                    pushed: false,
                });
            };

            match ctx.execute.jj.push_changes_create(
                &self.change_ids,
                Some(&self.remote),
                &push_argv,
            ) {
                Ok(()) => {
                    let changes = ctx.execute.jj.log(self.change_ids.join("|"))?;
                    let bookmarks: Vec<_> = Bookmark::from_changes(&changes).into_iter().collect();

                    ctx.execute.output.log_completed(&format!(
                        "Created bookmarks: {}",
                        bookmarks
                            .iter()
                            .map(|bookmark| bookmark.name().magenta().to_string())
                            .join(", ")
                    ));

                    Ok(ActionResultData::Pushed {
                        bookmarks: bookmarks
                            .iter()
                            .map(|bookmark| bookmark.name().to_owned())
                            .collect(),
                        created_bookmarks: bookmarks
                            .iter()
                            .map(|bookmark| {
                                (
                                    bookmark.change.change_id.clone(),
                                    bookmark.name().to_owned(),
                                )
                            })
                            .collect(),
                        pushed: true,
                    })
                }
                Err(error) => {
                    let error_msg = format!(
                        "Failed to create and push changes to remote {} ({change_ids_string}): {error}",
                        self.remote.cyan()
                    );
                    ctx.execute.output.log_message(&error_msg);
                    error!("{error_msg}");
                    Err(Error::new(error_msg))
                }
            }
        }
    }
}
