use std::collections::{BTreeMap, HashMap, HashSet};

use tempfile::TempDir;

use crate::{
    bookmark::{BookmarkGraph, BookmarkOrPending},
    config::{Config, ForgeType, RepoPushConfig},
    forge::{
        Forge as _,
        ForgeImpl,
        MergeRequestLike as _,
        test::{MergeRequest, TestForge},
    },
    jj::Jujutsu,
    output::BufferedOutput,
    submit::{
        ExecuteContext,
        PlanContext,
        RootExecuteContext,
        execute::{
            self,
            ActionResultData,
            ExecuteAction,
            ExecuteActionContext,
            SubmissionResult,
            push::PushAction,
            push_create::PushCreateAction,
        },
        find_changes_to_submit,
        plan::{self, SubmissionPlan},
    },
    tests::TestRepo,
};

async fn run_action<A>(
    action: A,
    push: RepoPushConfig,
    dry_run: bool,
    no_hooks: bool,
) -> (ActionResultData, String)
where
    A: ExecuteAction,
{
    let temp = TempDir::new().expect("temp dir");
    let jj = Jujutsu::new(temp.path()).expect("jj instance");
    let forge = ForgeImpl::Test(TestForge::default());
    let output = BufferedOutput::new();
    let config = Config::builder()
        .forge(ForgeType::Forgejo)
        .push(push)
        .build();
    let graph = BookmarkGraph::from_lookups(BTreeMap::new(), &BTreeMap::new());
    let plan = SubmissionPlan {
        actions: Vec::new(),
        existing_mrs: HashMap::new(),
    };
    let execute = ExecuteContext {
        jj: &jj,
        forge: &forge,
        config: &config,
        output: &output,
        bookmark_graph: &graph,
        dry_run,
        no_hooks,
        plan: &plan,
    };
    let ctx = ExecuteActionContext {
        execute,
        current_results: Vec::new(),
    };
    let result = action.execute(ctx).await.expect("push action succeeds");

    (result, output.get_buffer())
}

fn push_action() -> PushAction {
    PushAction {
        bookmarks: vec!["feature-a".to_owned()],
        remote: "origin".to_owned(),
    }
}

fn push_create_action() -> PushCreateAction {
    PushCreateAction {
        change_ids: vec!["abcdefgh".to_owned()],
        remote: "origin".to_owned(),
    }
}

fn pushed(result: ActionResultData) -> (Vec<String>, HashMap<String, String>, bool) {
    match result {
        ActionResultData::Pushed {
            bookmarks,
            created_bookmarks,
            pushed,
        } => (bookmarks, created_bookmarks, pushed),
        other => panic!("expected pushed result, got {other:?}"),
    }
}

#[tokio::test]
async fn custom_push_argv_runs_with_bookmark_arguments() {
    let temp = TempDir::new().expect("temp dir");
    let jj = Jujutsu::new(temp.path()).expect("jj instance");
    let forge = ForgeImpl::Test(TestForge::default());
    let output = BufferedOutput::new();
    let config = Config::builder()
        .forge(ForgeType::Forgejo)
        .push(RepoPushConfig::Command(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "printf '%s\\n' \"$0\" \"$@\" > push-args.txt".to_owned(),
        ]))
        .build();
    let graph = BookmarkGraph::from_lookups(BTreeMap::new(), &BTreeMap::new());
    let plan = SubmissionPlan {
        actions: Vec::new(),
        existing_mrs: HashMap::new(),
    };
    let ctx = ExecuteActionContext {
        execute: ExecuteContext {
            jj: &jj,
            forge: &forge,
            config: &config,
            output: &output,
            bookmark_graph: &graph,
            dry_run: false,
            no_hooks: false,
            plan: &plan,
        },
        current_results: Vec::new(),
    };

    let result = push_action()
        .execute(ctx)
        .await
        .expect("configured push command succeeds");

    let (_, _, did_push) = pushed(result);
    assert!(did_push, "a successful configured push is reported");
    let args = std::fs::read_to_string(temp.path().join("push-args.txt"))
        .expect("configured executable wrote its arguments");
    assert_eq!(args, "--remote\norigin\n--bookmark\n\"feature-a\"\n");
}

#[tokio::test]
async fn custom_push_create_argv_runs_with_change_arguments() {
    let temp = TempDir::new().expect("temp dir");
    let jj = Jujutsu::new(temp.path()).expect("jj instance");
    let forge = ForgeImpl::Test(TestForge::default());
    let output = BufferedOutput::new();
    let config = Config::builder()
        .forge(ForgeType::Forgejo)
        .push(RepoPushConfig::Command(vec![
            "sh".to_owned(),
            "-c".to_owned(),
            "printf '%s\\n' \"$0\" \"$@\" > push-args.txt; exec jj git push \"$0\" \"$@\""
                .to_owned(),
        ]))
        .build();
    jj.exec(["git", "init"]).expect("initialize jj repository");
    jj.exec(["config", "set", "--repo", "user.name", "Test User"])
        .expect("configure jj username");
    jj.exec(["config", "set", "--repo", "user.email", "test@example.com"])
        .expect("configure jj email");
    std::fs::write(temp.path().join("README.md"), "initial commit\n").expect("write initial file");
    jj.exec(["describe", "-m", "Initial commit"])
        .expect("create initial commit");

    let remote_dir = temp.path().join("remote.git");
    std::fs::create_dir_all(&remote_dir).expect("create bare remote directory");
    let remote = Jujutsu::new(&remote_dir).expect("remote jj instance");
    remote
        .exec(["git", "init"])
        .expect("initialize bare remote");
    jj.exec([
        "git",
        "remote",
        "add",
        "origin",
        &remote_dir.to_string_lossy(),
    ])
    .expect("add push remote");
    let change_id = jj
        .log("@")
        .expect("read initial change")
        .into_iter()
        .next()
        .expect("initial change exists")
        .change_id;

    let action = PushCreateAction {
        change_ids: vec![change_id.clone()],
        remote: "origin".to_owned(),
    };
    let graph = BookmarkGraph::from_lookups(BTreeMap::new(), &BTreeMap::new());
    let plan = SubmissionPlan {
        actions: Vec::new(),
        existing_mrs: HashMap::new(),
    };
    let ctx = ExecuteActionContext {
        execute: ExecuteContext {
            jj: &jj,
            forge: &forge,
            config: &config,
            output: &output,
            bookmark_graph: &graph,
            dry_run: false,
            no_hooks: false,
            plan: &plan,
        },
        current_results: Vec::new(),
    };

    let result = action
        .execute(ctx)
        .await
        .expect("configured create-push command succeeds");

    let (_, _, did_push) = pushed(result);
    assert!(did_push, "a successful configured push is reported");
    let args = std::fs::read_to_string(temp.path().join("push-args.txt"))
        .expect("configured executable wrote its arguments");
    assert_eq!(args, format!("--remote\norigin\n-c\n{change_id}\n"));
}

#[tokio::test]
async fn no_hooks_uses_builtin_push_command() {
    let (result, output) = run_action(
        push_action(),
        RepoPushConfig::Command(vec!["custom-push".to_owned(), "push".to_owned()]),
        true,
        true,
    )
    .await;

    let (_, _, did_push) = pushed(result);
    assert!(
        did_push,
        "a dry-run with pushing enabled reports a planned push"
    );
    assert!(output.contains("via `jj git push`"));
    assert!(!output.contains("custom-push"));
}

#[tokio::test]
async fn disabled_push_is_not_reported_even_with_no_hooks() {
    let (result, output) =
        run_action(push_action(), RepoPushConfig::Enabled(false), true, true).await;

    let (_, _, did_push) = pushed(result);
    assert!(!did_push, "disabled pushing stays disabled with no-hooks");
    assert!(output.contains("(pushing disabled)"));
}

#[tokio::test]
async fn disabled_create_push_reports_no_bookmarks() {
    let (result, output) = run_action(
        push_create_action(),
        RepoPushConfig::Enabled(false),
        true,
        false,
    )
    .await;

    let (bookmarks, created_bookmarks, did_push) = pushed(result);
    assert!(!did_push);
    assert!(bookmarks.is_empty());
    assert!(created_bookmarks.is_empty());
    assert!(output.contains("(pushing disabled)"));
}

#[tokio::test]
async fn disabled_real_push_returns_without_running_jj() {
    let (result, _) = run_action(push_action(), RepoPushConfig::Enabled(false), false, false).await;

    let (_, _, did_push) = pushed(result);
    assert!(!did_push);
}

#[tokio::test]
async fn dry_run_does_not_disclose_configured_push_arguments() {
    let sentinel = "push-sentinel-secret-value";
    let (result, output) = run_action(
        push_action(),
        RepoPushConfig::Command(vec!["custom-push".to_owned(), sentinel.to_owned()]),
        true,
        false,
    )
    .await;

    let (_, _, did_push) = pushed(result);
    assert!(did_push, "an enabled dry-run reports a planned push");
    assert!(output.contains("via configured push command"));
    assert!(!output.contains(sentinel));
}

#[tokio::test]
async fn create_dry_run_does_not_disclose_configured_push_arguments() {
    let sentinel = "create-push-sentinel-secret";
    let (result, output) = run_action(
        push_create_action(),
        RepoPushConfig::Command(vec!["custom-push".to_owned(), sentinel.to_owned()]),
        true,
        false,
    )
    .await;

    let (_, _, did_push) = pushed(result);
    assert!(did_push, "an enabled dry-run reports a planned create-push");
    assert!(output.contains("via configured push command"));
    assert!(!output.contains(sentinel));
}

/// A repository with two stacks:
///
/// - `stale` was pushed with an MR into `main`, then rewritten locally, and
///   `child` (never pushed) sits on top of it.
/// - `synced` was pushed and is unchanged locally; its MR targets an old base.
///
/// Returns the repository and a forge holding the MRs for `stale` and
/// `synced`.
fn stale_and_synced_stacks() -> (TestRepo<TestRepo<()>>, ForgeImpl) {
    let repo = TestRepo::with_local_remote();

    repo.create_change("stale.txt", "stale", "Stale commit")
        .create_and_push_bookmark("stale");
    repo.exec(["describe", "-r", "stale", "-m", "Stale commit, rewritten"]);
    repo.new_on("stale")
        .create_change("child.txt", "child", "Child commit")
        .create_bookmark("child");

    repo.new_on("main")
        .create_change("synced.txt", "synced", "Synced commit")
        .create_and_push_bookmark("synced");
    repo.new_on("main");

    let forge = ForgeImpl::Test(
        TestForge::builder()
            .merge_requests(HashMap::from([
                (
                    "1".to_owned(),
                    MergeRequest::builder()
                        .id("1".to_owned())
                        .title("Stale commit".to_owned())
                        .source_branch("stale".to_owned())
                        .target_branch("main".to_owned())
                        .build(),
                ),
                (
                    "2".to_owned(),
                    MergeRequest::builder()
                        .id("2".to_owned())
                        .title("Synced commit".to_owned())
                        .description("Synced description".to_owned())
                        .source_branch("synced".to_owned())
                        .target_branch("old-base".to_owned())
                        .build(),
                ),
            ]))
            .build(),
    );

    (repo, forge)
}

/// Plans and executes a submission of `child` and `synced`, as `submit` does.
async fn plan_and_execute(
    repo: &TestRepo<TestRepo<()>>,
    forge: &ForgeImpl,
    push: RepoPushConfig,
    dry_run: bool,
) -> (SubmissionResult, String) {
    let config = Config::builder()
        .forge(ForgeType::Forgejo)
        .push(push)
        .build();
    let output = BufferedOutput::new();
    let pending = HashSet::new();
    let targets = repo
        .jj
        .log("child | synced")
        .expect("read submission targets");
    let bookmarks: Vec<_> = BookmarkOrPending::from_changes(&targets)
        .into_iter()
        .collect();
    let changes = find_changes_to_submit(
        &repo.jj,
        bookmarks.iter().map(BookmarkOrPending::change_id),
        &pending,
    )
    .expect("find changes to submit");
    let graph = BookmarkGraph::from_changes(&repo.jj, &changes, false).expect("build graph");

    let submission_plan = plan::plan(PlanContext {
        jj: &repo.jj,
        forge,
        config: &config,
        output: &output,
        bookmark_graph: &graph,
        dry_run,
    })
    .await
    .expect("plan submission");

    let result = execute::execute(RootExecuteContext::new(
        &repo.jj,
        forge,
        &config,
        &output,
        dry_run,
        submission_plan,
        changes.clone(),
        false,
        false,
    ))
    .await
    .expect("execute submission");

    (
        result,
        strip_ansi::strip_str(&output.get_buffer()).to_string(),
    )
}

async fn mr_target(forge: &ForgeImpl, source_branch: &str) -> Option<String> {
    forge
        .find_merge_request_by_source_branch(source_branch)
        .await
        .expect("query test forge")
        .map(|mr| mr.target_branch().to_owned())
}

async fn mr_description(forge: &ForgeImpl, source_branch: &str) -> String {
    forge
        .find_merge_request_by_source_branch(source_branch)
        .await
        .expect("query test forge")
        .expect("merge request exists")
        .description()
        .to_owned()
}

#[tokio::test]
async fn disabled_push_skips_mr_actions_on_unpushed_heads() {
    let (repo, forge) = stale_and_synced_stacks();
    let stale_description = mr_description(&forge, "stale").await;

    let (result, output) =
        plan_and_execute(&repo, &forge, RepoPushConfig::Enabled(false), false).await;

    assert!(
        result.errors.is_empty(),
        "disabled push is not an error: {:?}",
        result.errors
    );
    assert!(result.bookmarks_pushed.is_empty());
    assert_eq!(
        mr_target(&forge, "child").await,
        None,
        "no MR is opened for a branch that was never pushed"
    );
    assert_eq!(
        mr_description(&forge, "stale").await,
        stale_description,
        "the MR of a stale remote head is left untouched"
    );
    assert!(
        result
            .merge_requests
            .iter()
            .all(|update| update.bookmark == "synced"),
        "only the synced stack reports MR changes"
    );
    assert!(output.contains("Skipping because pushing is disabled"));
    assert!(output.contains("Create MR for child"));

    assert_eq!(
        mr_target(&forge, "synced").await.as_deref(),
        Some("main"),
        "an MR whose branch is already pushed is still retargeted"
    );
}

#[tokio::test]
async fn disabled_push_dry_run_reports_skipped_mr_actions() {
    let (repo, forge) = stale_and_synced_stacks();

    let (result, output) =
        plan_and_execute(&repo, &forge, RepoPushConfig::Enabled(false), true).await;

    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(output.contains("Would skip because pushing is disabled"));
    assert!(
        !output.contains("Would create MR child"),
        "dry-run does not promise an MR that the real run would skip"
    );
    assert!(output.contains("Would update MR #2 base for synced to main"));
    assert_eq!(mr_target(&forge, "child").await, None);
    assert_eq!(
        mr_target(&forge, "synced").await.as_deref(),
        Some("old-base")
    );
}

#[tokio::test]
async fn failed_push_still_fails_dependent_mr_actions() {
    let (repo, forge) = stale_and_synced_stacks();

    let (result, _) = plan_and_execute(
        &repo,
        &forge,
        RepoPushConfig::Command(vec!["false".to_owned()]),
        false,
    )
    .await;

    assert!(
        result
            .errors
            .iter()
            .any(|error| error.to_string().contains("Dependencies failed")),
        "a failed push is reported, not skipped: {:?}",
        result.errors
    );
    assert_eq!(mr_target(&forge, "child").await, None);
}
