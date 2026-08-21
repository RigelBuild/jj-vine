use std::collections::HashMap;

use bon::Builder;
use itertools::Itertools as _;
use owo_colors::OwoColorize as _;
use tracing::{debug, error};

use crate::{
    error::{Error, Result},
    submit::execute::{ActionInfo, ActionResultData, ExecuteAction, ExecuteActionContext},
};

/// Push bookmarks to a remote.
#[derive(Debug, Clone, PartialEq, Eq, Builder)]
pub struct PushAction {
    pub bookmarks: Vec<String>,

    pub remote: String,
}

impl ActionInfo for PushAction {
    fn id(&self) -> String {
        format!("push:{}", self.bookmarks.join(","))
    }

    fn group_text(&self) -> String {
        "Pushing bookmarks".to_owned()
    }

    fn text(&self) -> String {
        format!(
            "Pushing {}",
            self.bookmarks.iter().map(|b| b.magenta()).join(", ")
        )
    }

    fn substep_text(&self) -> String {
        self.bookmarks.iter().map(|b| b.magenta()).join(", ")
    }

    fn plan_text(&self) -> String {
        format!(
            "Push bookmarks to remote {}: {}",
            self.remote.cyan(),
            self.bookmarks.iter().map(|b| b.magenta()).join(", "),
        )
    }
}

impl ExecuteAction for PushAction {
    fn execute(
        &self,
        ctx: ExecuteActionContext<'_>,
    ) -> impl Future<Output = Result<ActionResultData>> {
        let bookmarks_string = self.bookmarks.iter().map(|b| b.magenta()).join(", ");

        // Resolve the push command once, up front. `--no-hooks` bypasses a
        // configured gate command (e.g. `["jj-hp","push"]` routing through the
        // jj-hooks pre-push gate) by forcing the built-in `jj git push` for
        // this run; it does NOT re-enable a push disabled by config. `None`
        // means pushing is disabled (`jj-vine.push = false`) — a no-op success.
        let push_argv = ctx.execute.config.push.resolve_argv(ctx.execute.no_hooks);

        core::future::ready(if ctx.execute.dry_run {
            let plan = push_argv.as_ref().map_or_else(
                || "(pushing disabled)".to_owned(),
                |argv| format!("via `{}`", argv.join(" ")),
            );
            ctx.execute.output.log_message(&format!(
                "Would push bookmarks to remote {} {plan}: {bookmarks_string}",
                self.remote.cyan()
            ));

            Ok(ActionResultData::Pushed {
                bookmarks: self.bookmarks.clone(),
                created_bookmarks: HashMap::new(),
                pushed: push_argv.is_some(),
            })
        } else {
            let Some(push_argv) = push_argv else {
                debug!("Pushing disabled (jj-vine.push = false); skipping {bookmarks_string}");
                return core::future::ready(Ok(ActionResultData::Pushed {
                    bookmarks: self.bookmarks.clone(),
                    created_bookmarks: HashMap::new(),
                    pushed: false,
                }));
            };
            match ctx
                .execute
                .jj
                .push_bookmarks(&self.bookmarks, Some(&self.remote), &push_argv)
            {
                Ok(pushed) => {
                    if pushed {
                        ctx.execute
                            .output
                            .log_completed(&format!("Pushed {bookmarks_string}"));
                    } else {
                        debug!("Nothing needed to be pushed for {bookmarks_string}");
                    }

                    Ok(ActionResultData::Pushed {
                        bookmarks: self.bookmarks.clone(),
                        created_bookmarks: HashMap::new(),
                        pushed,
                    })
                }
                Err(e) => {
                    let error_msg = format!("Failed to push {bookmarks_string}: {e}");
                    ctx.execute.output.log_message(&error_msg);
                    error!("{error_msg}");
                    Err(Error::new(error_msg))
                }
            }
        })
    }
}
