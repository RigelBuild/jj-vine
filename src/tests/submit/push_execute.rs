use std::collections::{BTreeMap, HashMap};

use tempfile::TempDir;

use crate::{
    bookmark::BookmarkGraph,
    config::{Config, ForgeType, RepoPushConfig},
    forge::{ForgeImpl, test::TestForge},
    jj::Jujutsu,
    output::BufferedOutput,
    submit::{
        ExecuteContext,
        execute::{
            ActionResultData,
            ExecuteAction,
            ExecuteActionContext,
            push::PushAction,
            push_create::PushCreateAction,
        },
        plan::SubmissionPlan,
    },
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
