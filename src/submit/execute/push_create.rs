use std::collections::HashMap;

use bon::Builder;
use itertools::Itertools as _;
use owo_colors::OwoColorize as _;
use tracing::{debug, error};

use crate::{
    bookmark::{Bookmark, change_id_to_temp_bookmark_name},
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
            .map(|b| (&b[..8]).magenta().to_string())
            .join(", ");

        // Resolve the push command once, up front, matching PushAction:
        // `--no-hooks` bypasses a configured gate command (e.g. the `jj-hp`
        // gate) by forcing the built-in `jj git push` for this run; it does
        // NOT re-enable a push disabled by config. `None` means pushing is
        // disabled (`jj-vine.push = false`) — a no-op success, no bookmarks
        // created.
        let push_argv = ctx.execute.config.push.resolve_argv(ctx.execute.no_hooks);

        if ctx.execute.dry_run {
            let plan = push_argv.as_ref().map_or_else(
                || "(pushing disabled)".to_owned(),
                |argv| format!("via `{}`", argv.join(" ")),
            );
            ctx.execute.output.log_message(&format!(
                "Would {} and push to remote {} {plan} for changes: {change_ids_string}",
                "create bookmarks".green(),
                self.remote.cyan()
            ));

            // Mirror the real disabled path (below): a disabled push (`None`)
            // creates and pushes nothing, so its preview must too — otherwise
            // the summary reports temp bookmarks as pushed, contradicting the
            // `(pushing disabled)` plan line and previewing a creation a real
            // run never performs.
            if push_argv.is_some() {
                Ok(ActionResultData::Pushed {
                    bookmarks: self
                        .change_ids
                        .iter()
                        .map(|c| change_id_to_temp_bookmark_name(c))
                        .collect(),
                    created_bookmarks: self
                        .change_ids
                        .iter()
                        .map(|c| (c.clone(), change_id_to_temp_bookmark_name(c)))
                        .collect(),
                    pushed: true,
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
                debug!(
                    "Pushing disabled (jj-vine.push = false); skipping create+push {change_ids_string}"
                );
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
                            .map(|b| b.name().magenta().to_string())
                            .join(", ")
                    ));

                    Ok(ActionResultData::Pushed {
                        bookmarks: bookmarks.iter().map(|b| b.name().to_owned()).collect(),
                        created_bookmarks: bookmarks
                            .iter()
                            .map(|b| (b.change.change_id.clone(), b.name().to_owned()))
                            .collect(),
                        pushed: true,
                    })
                }
                Err(e) => {
                    let error_msg = format!(
                        "Failed to create and push changes to remote {} ({change_ids_string}): {e}",
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
