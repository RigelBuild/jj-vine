use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex},
};

use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
    sync::oneshot,
};

use crate::{
    bookmark::{BookmarkGraph, BookmarkOrPending, change_id_to_temp_bookmark_name},
    config::{Config, ForgeType, GitLabConfig, RepoPushConfig, TitleConfig},
    forge::{
        Forge as _,
        ForgeImpl,
        MergeRequestLike as _,
        gitlab::GitLabForge,
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

/// Submission revset for [`stale_and_synced_stacks`].
const STALE_AND_SYNCED: &str = "child | synced";

/// Plans and executes a submission of `revset`, as `submit` does. Changes in
/// `pending` get bookmarks created, as with `submit --create`.
async fn plan_and_execute(
    repo: &TestRepo<TestRepo<()>>,
    forge: &ForgeImpl,
    push: RepoPushConfig,
    dry_run: bool,
    revset: &str,
    pending: &HashSet<String>,
) -> (SubmissionResult, String) {
    let config = Config::builder()
        .forge(ForgeType::Forgejo)
        .push(push)
        .build();
    plan_and_execute_with_config(repo, forge, &config, dry_run, revset, pending).await
}

async fn plan_and_execute_with_config(
    repo: &TestRepo<TestRepo<()>>,
    forge: &ForgeImpl,
    config: &Config,
    dry_run: bool,
    revset: &str,
    pending: &HashSet<String>,
) -> (SubmissionResult, String) {
    let output = BufferedOutput::new();
    let targets = repo
        .jj
        .log_with_pending_bookmarks(revset, pending)
        .expect("read submission targets");
    let bookmarks: Vec<_> = BookmarkOrPending::from_changes(&targets)
        .into_iter()
        .collect();
    let changes = find_changes_to_submit(
        &repo.jj,
        bookmarks.iter().map(BookmarkOrPending::change_id),
        pending,
    )
    .expect("find changes to submit");
    let graph = BookmarkGraph::from_changes(&repo.jj, &changes, false).expect("build graph");

    let submission_plan = plan::plan(PlanContext {
        jj: &repo.jj,
        forge,
        config,
        output: &output,
        bookmark_graph: &graph,
        dry_run,
    })
    .await
    .expect("plan submission");

    let result = execute::execute(RootExecuteContext::new(
        &repo.jj,
        forge,
        config,
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

fn mock_gitlab_forge(api: &GitLabApiMock) -> ForgeImpl {
    ForgeImpl::GitLab(
        GitLabForge::new(
            &api.base_url,
            "group/project",
            "group/project",
            "test-token",
            None::<&str>,
            false,
            true,
        )
        .expect("create mock GitLab forge"),
    )
}

fn gitlab_disabled_push_config() -> Config {
    Config::builder()
        .forge(ForgeType::GitLab)
        .push(RepoPushConfig::Enabled(false))
        .gitlab(GitLabConfig {
            create_merge_request_dependencies: true,
            ..GitLabConfig::default()
        })
        .title(TitleConfig {
            sync_single_revision: false,
            sync_multiple_revisions: false,
            ..TitleConfig::default()
        })
        .build()
}

const MR_API: &str = "/api/v4/projects/group%2Fproject/merge_requests";

#[tokio::test]
async fn disabled_push_syncs_child_dependencies_after_skipped_parent_base_update() {
    for dry_run in [false, true] {
        let (repo, pending_change_id) = pending_parent_synced_stacks();
        let api = GitLabApiMock::start(vec![
            gitlab_mr(1, 101, "p", "main"),
            gitlab_mr(2, 102, "c", "p"),
            gitlab_mr(3, 103, "synced", "old-base"),
        ])
        .await;
        let forge = mock_gitlab_forge(&api);

        let (result, output) = plan_and_execute_with_config(
            &repo,
            &forge,
            &gitlab_disabled_push_config(),
            dry_run,
            &format!("{pending_change_id} | c | synced"),
            &HashSet::from([pending_change_id]),
        )
        .await;
        let requests = api.requests();
        api.stop().await;

        let skip_verb = if dry_run { "Would skip" } else { "Skipping" };
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.bookmarks_pushed.is_empty());
        assert!(output.contains(&format!(
            "{skip_verb} because pushing is disabled: Update base of MR 1 for p"
        )));
        assert!(output.contains(&format!(
            "{skip_verb} because pushing is disabled: Regenerate stack in description of MR for c"
        )));
        assert!(
            !output.contains("because pushing is disabled: Sync dependent merge requests for c"),
            "a skipped update of an existing parent MR does not skip the child's sync: {output}"
        );
        assert!(
            !requests
                .iter()
                .any(|request| request.line.starts_with(&format!("PUT {MR_API}/1 "))),
            "the skipped parent base update must not call the GitLab update API: {requests:?}"
        );

        if dry_run {
            assert!(output.contains("Would update MR !3 base for synced to main"));
            assert!(
                requests
                    .iter()
                    .all(|request| request.line.starts_with("GET ")),
                "dry-run only reads: {requests:?}"
            );
        } else {
            assert!(
                requests
                    .iter()
                    .any(|request| request.line.starts_with(&format!("PUT {MR_API}/3 ")))
            );
            assert_eq!(
                dependency_posts(&requests),
                vec![(2, json!({ "blocking_merge_request_id": 101 }))],
                "c's MR is made dependent on p's existing MR: {requests:?}"
            );
        }
    }
}

#[tokio::test]
async fn disabled_push_syncs_pushed_chain_dependencies_below_pending_descendant() {
    for dry_run in [false, true] {
        let (repo, pending_change_id) = pushed_chain_with_pending_descendant();
        let pending_name = change_id_to_temp_bookmark_name(&pending_change_id);
        let api = GitLabApiMock::start(vec![
            gitlab_mr(1, 101, "a", "main"),
            gitlab_mr(2, 102, "b", "a"),
        ])
        .await;
        let forge = mock_gitlab_forge(&api);

        let (result, output) = plan_and_execute_with_config(
            &repo,
            &forge,
            &gitlab_disabled_push_config(),
            dry_run,
            &pending_change_id,
            &HashSet::from([pending_change_id.clone()]),
        )
        .await;
        let requests = api.requests();
        api.stop().await;

        let skip_verb = if dry_run { "Would skip" } else { "Skipping" };
        assert!(result.errors.is_empty(), "{:?}", result.errors);
        assert!(result.bookmarks_pushed.is_empty());
        for skipped in [
            format!("Create MR for {pending_name}"),
            "Regenerate stack in description of MR for a".to_owned(),
            "Regenerate stack in description of MR for b".to_owned(),
            format!("Sync dependent merge requests for {pending_name}"),
        ] {
            assert!(
                output.contains(&format!(
                    "{skip_verb} because pushing is disabled: {skipped}"
                )),
                "expected skipped action {skipped:?}: {output}"
            );
        }
        assert!(
            !output.contains("because pushing is disabled: Sync dependent merge requests for b"),
            "b and its direct parent a both have MRs, so b's sync runs: {output}"
        );
        assert!(
            !requests
                .iter()
                .any(|request| request.line.starts_with("PUT ")),
            "no MR description or base is updated: {requests:?}"
        );

        if dry_run {
            assert!(
                requests
                    .iter()
                    .all(|request| request.line.starts_with("GET ")),
                "dry-run only reads: {requests:?}"
            );
        } else {
            assert_eq!(
                dependency_posts(&requests),
                vec![(2, json!({ "blocking_merge_request_id": 101 }))],
                "b's MR is made dependent on a's MR: {requests:?}"
            );
            assert!(
                result
                    .merge_requests
                    .iter()
                    .any(|update| update.bookmark == "b"),
                "b reports its dependency sync: {:?}",
                result.merge_requests
            );
        }
    }
}

/// MR IIDs and JSON bodies of dependency-creation requests, in request order.
fn dependency_posts(requests: &[MockRequest]) -> Vec<(u64, Value)> {
    requests
        .iter()
        .filter_map(|request| {
            let path = request.line.strip_prefix(&format!("POST {MR_API}/"))?;
            let iid = path.split_once("/blocks ")?.0.parse().ok()?;
            let body = serde_json::from_str(&request.body).expect("dependency body is JSON");
            Some((iid, body))
        })
        .collect()
}

/// A pushed stack `main -> a -> b` with an unbookmarked change on `b`, as
/// `submit --create` would bookmark. Returns the repository and the pending
/// change ID.
fn pushed_chain_with_pending_descendant() -> (TestRepo<TestRepo<()>>, String) {
    let repo = TestRepo::with_local_remote();

    repo.create_change("a.txt", "a", "A commit")
        .create_and_push_bookmark("a");
    repo.new_on("a")
        .create_change("b.txt", "b", "B commit")
        .create_and_push_bookmark("b");
    repo.new_on("b")
        .create_change("pending.txt", "pending", "Pending commit");
    let pending_change_id = repo
        .jj
        .log("@")
        .expect("read pending change")
        .into_iter()
        .next()
        .expect("pending change exists")
        .change_id;
    repo.new_on("main");

    (repo, pending_change_id)
}

fn pending_parent_synced_stacks() -> (TestRepo<TestRepo<()>>, String) {
    let repo = TestRepo::with_local_remote();

    repo.create_change("pending.txt", "pending", "Pending commit");
    let pending_change_id = repo
        .jj
        .log("@")
        .expect("read pending change")
        .into_iter()
        .next()
        .expect("pending change exists")
        .change_id;
    repo.jj.exec(["new"]).expect("continue pending stack");
    repo.create_change("parent.txt", "parent", "Parent commit")
        .create_and_push_bookmark("p");
    repo.jj.exec(["new", "p"]).expect("start child stack");
    repo.create_change("child.txt", "child", "Child commit")
        .create_and_push_bookmark("c");
    repo.jj
        .exec(["new", "main"])
        .expect("start independent stack");
    repo.create_change("synced.txt", "synced", "Synced commit")
        .create_and_push_bookmark("synced");
    repo.new_on("main");

    (repo, pending_change_id)
}

/// A request received by [`GitLabApiMock`].
#[derive(Debug, Clone)]
struct MockRequest {
    /// The HTTP request line, such as `GET /path HTTP/1.1`.
    line: String,
    body: String,
}

/// A minimal GitLab API serving a fixed set of merge requests.
struct GitLabApiMock {
    base_url: String,
    requests: Arc<Mutex<Vec<MockRequest>>>,
    shutdown: Option<oneshot::Sender<()>>,
    server: tokio::task::JoinHandle<()>,
}

impl GitLabApiMock {
    async fn start(merge_requests: Vec<Value>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mock GitLab API");
        let address = listener.local_addr().expect("read mock address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let server_requests = Arc::clone(&requests);
        let (shutdown, mut shutdown_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            loop {
                let accepted = tokio::select! {
                    result = listener.accept() => result,
                    _ = &mut shutdown_rx => break,
                };
                let Ok((mut stream, _)) = accepted else {
                    break;
                };

                let request = read_mock_request(&mut stream).await;
                let body = gitlab_mock_response(&merge_requests, &request).to_string();
                server_requests
                    .lock()
                    .expect("lock request log")
                    .push(request);
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("write mock GitLab response");
            }
        });

        Self {
            base_url: format!("http://{address}"),
            requests,
            shutdown: Some(shutdown),
            server,
        }
    }

    fn requests(&self) -> Vec<MockRequest> {
        self.requests.lock().expect("lock request log").clone()
    }

    async fn stop(mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown.send(()).expect("signal mock GitLab shutdown");
        }
        self.server.await.expect("stop mock GitLab API");
    }
}

/// Reads one HTTP request, including the whole body named by
/// `Content-Length`, so the client never sees the socket close mid-send.
async fn read_mock_request(stream: &mut tokio::net::TcpStream) -> MockRequest {
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    let header_end = loop {
        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
        let read = stream
            .read(&mut buffer)
            .await
            .expect("read mock GitLab request");
        assert_ne!(read, 0, "connection closed before request headers ended");
        request.extend_from_slice(&buffer[..read]);
    };

    let headers = String::from_utf8(request[..header_end].to_vec()).expect("headers are UTF-8");
    let content_length = headers
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.trim().eq_ignore_ascii_case("content-length"))
        .map_or(0, |(_, value)| {
            value
                .trim()
                .parse::<usize>()
                .expect("numeric Content-Length")
        });
    while request.len() < header_end + content_length {
        let read = stream
            .read(&mut buffer)
            .await
            .expect("read mock GitLab request body");
        assert_ne!(read, 0, "connection closed before request body ended");
        request.extend_from_slice(&buffer[..read]);
    }

    MockRequest {
        line: headers
            .lines()
            .next()
            .expect("HTTP request line")
            .to_owned(),
        body: String::from_utf8(request[header_end..header_end + content_length].to_vec())
            .expect("body is UTF-8"),
    }
}

/// The mock GitLab response for `request` over `merge_requests`.
fn gitlab_mock_response(merge_requests: &[Value], request: &MockRequest) -> Value {
    let mut parts = request.line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or_default();
    let Some(rest) = target.strip_prefix(MR_API) else {
        return json!([]);
    };
    let by_iid = |iid: &str| {
        merge_requests
            .iter()
            .find(|mr| mr["iid"].to_string() == iid)
            .cloned()
            .unwrap_or_else(|| panic!("unknown mock MR {iid}"))
    };

    if let Some(query) = rest.strip_prefix('?') {
        let source = query
            .split('&')
            .find_map(|pair| pair.strip_prefix("source_branch="))
            .unwrap_or_default();
        return Value::Array(
            merge_requests
                .iter()
                .filter(|mr| mr["source_branch"] == source)
                .cloned()
                .collect(),
        );
    }

    let path = rest.trim_start_matches('/');
    match (method, path.split_once('/')) {
        ("GET", Some((_, "blocks"))) => json!([]),
        ("POST", Some((_, "blocks"))) => {
            let body: Value = serde_json::from_str(&request.body).expect("JSON request body");
            let blocking = merge_requests
                .iter()
                .find(|mr| mr["id"] == body["blocking_merge_request_id"])
                .cloned()
                .expect("blocking MR exists");
            json!({ "id": 900, "blocking_merge_request": blocking, "project_id": 1 })
        }
        ("GET", None) => by_iid(path),
        ("PUT", None) => {
            let mut mr = by_iid(path);
            let body: Value = serde_json::from_str(&request.body).expect("JSON request body");
            if let (Some(mr), Some(body)) = (mr.as_object_mut(), body.as_object()) {
                mr.extend(body.clone());
            }
            mr
        }
        _ => json!([]),
    }
}

fn gitlab_mr(iid: u64, id: u64, source_branch: &str, target_branch: &str) -> Value {
    json!({
        "iid": iid,
        "id": id,
        "title": format!("MR {source_branch}"),
        "description": "description",
        "source_branch": source_branch,
        "target_branch": target_branch,
        "state": "opened",
        "web_url": format!("https://gitlab.example.com/group/project/-/merge_requests/{iid}"),
        "author": { "id": 1, "username": "test" },
        "created_at": "2026-01-01T00:00:00Z",
        "assignees": [],
        "reviewers": []
    })
}

#[tokio::test]
async fn disabled_push_skips_mr_actions_on_unpushed_heads() {
    let (repo, forge) = stale_and_synced_stacks();
    let stale_description = mr_description(&forge, "stale").await;

    let (result, output) = plan_and_execute(
        &repo,
        &forge,
        RepoPushConfig::Enabled(false),
        false,
        STALE_AND_SYNCED,
        &HashSet::new(),
    )
    .await;

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

    let (result, output) = plan_and_execute(
        &repo,
        &forge,
        RepoPushConfig::Enabled(false),
        true,
        STALE_AND_SYNCED,
        &HashSet::new(),
    )
    .await;

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
        STALE_AND_SYNCED,
        &HashSet::new(),
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

/// A repository with a second remote, `mirror`, beside the configured push
/// remote `origin`:
///
/// - `missing` is tracked and synced on `mirror` but was never pushed to
///   `origin`.
/// - `stale` was pushed to `origin`, then rewritten and pushed to `mirror`
///   only, so it is synced on `mirror` and stale on `origin`.
///
/// Both have an MR targeting an old base. Returns the repository, the
/// `mirror` remote (kept alive for the test), and the forge.
fn synced_only_on_other_remote() -> (TestRepo<TestRepo<()>>, TestRepo<()>, ForgeImpl) {
    let repo = TestRepo::with_local_remote();
    let mirror = TestRepo::new();
    repo.exec([
        "git",
        "remote",
        "add",
        "mirror",
        mirror.path.to_str().expect("UTF-8 temp path"),
    ]);

    repo.create_change("missing.txt", "missing", "Missing commit")
        .create_bookmark("missing")
        .exec(["bookmark", "track", "missing", "--remote", "mirror"])
        .exec(["git", "push", "--remote", "mirror", "--bookmark", "missing"]);

    repo.new_on("main")
        .create_change("stale.txt", "stale", "Stale commit")
        .create_and_push_bookmark("stale")
        .exec(["describe", "-r", "stale", "-m", "Stale commit, rewritten"])
        .exec(["bookmark", "track", "stale", "--remote", "mirror"])
        .exec(["git", "push", "--remote", "mirror", "--bookmark", "stale"]);
    repo.new_on("main");

    let forge = ForgeImpl::Test(
        TestForge::builder()
            .merge_requests(HashMap::from([
                (
                    "1".to_owned(),
                    MergeRequest::builder()
                        .id("1".to_owned())
                        .title("Missing commit".to_owned())
                        .source_branch("missing".to_owned())
                        .target_branch("old-base".to_owned())
                        .build(),
                ),
                (
                    "2".to_owned(),
                    MergeRequest::builder()
                        .id("2".to_owned())
                        .title("Stale commit".to_owned())
                        .source_branch("stale".to_owned())
                        .target_branch("old-base".to_owned())
                        .build(),
                ),
            ]))
            .build(),
    );

    (repo, mirror, forge)
}

#[tokio::test]
async fn disabled_push_ignores_sync_with_a_remote_other_than_the_push_remote() {
    let (repo, _mirror, forge) = synced_only_on_other_remote();

    let (result, output) = plan_and_execute(
        &repo,
        &forge,
        RepoPushConfig::Enabled(false),
        false,
        "missing | stale",
        &HashSet::new(),
    )
    .await;

    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(result.bookmarks_pushed.is_empty());
    assert!(
        result.merge_requests.is_empty(),
        "no MR changes for heads missing or stale on the push remote: {:?}",
        result.merge_requests
    );
    assert!(output.contains("Skipping because pushing is disabled"));
    assert_eq!(
        mr_target(&forge, "missing").await.as_deref(),
        Some("old-base"),
        "sync with mirror does not make a head missing on origin pushed"
    );
    assert_eq!(
        mr_target(&forge, "stale").await.as_deref(),
        Some("old-base"),
        "sync with mirror does not make a stale head on origin pushed"
    );
}

/// A repository with one unbookmarked change on `main`, as `submit --create`
/// would bookmark. Returns the repository and the change ID.
fn unbookmarked_change() -> (TestRepo<TestRepo<()>>, String) {
    let repo = TestRepo::with_local_remote();
    repo.create_change("new.txt", "new", "New commit");
    let change_id = repo
        .jj
        .log("@")
        .expect("read new change")
        .into_iter()
        .next()
        .expect("new change exists")
        .change_id;
    repo.new_on("main");

    (repo, change_id)
}

#[tokio::test]
async fn disabled_push_create_skips_mr_creation() {
    let (repo, change_id) = unbookmarked_change();
    let forge = ForgeImpl::Test(TestForge::default());

    let (result, output) = plan_and_execute(
        &repo,
        &forge,
        RepoPushConfig::Enabled(false),
        false,
        &change_id,
        &HashSet::from([change_id.clone()]),
    )
    .await;

    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(result.bookmarks_pushed.is_empty());
    assert!(result.merge_requests.is_empty(), "no MR is created");
    assert!(output.contains("Skipping because pushing is disabled"));
    let change = repo
        .jj
        .log(&change_id)
        .expect("read change")
        .into_iter()
        .next()
        .expect("change exists");
    assert!(
        change.bookmarks.is_empty(),
        "no bookmark is created when pushing is disabled: {:?}",
        change.bookmarks
    );
}

#[tokio::test]
async fn disabled_push_create_dry_run_promises_no_push_or_mr() {
    let (repo, change_id) = unbookmarked_change();
    let forge = ForgeImpl::Test(TestForge::default());

    let (result, output) = plan_and_execute(
        &repo,
        &forge,
        RepoPushConfig::Enabled(false),
        true,
        &change_id,
        &HashSet::from([change_id.clone()]),
    )
    .await;

    assert!(result.errors.is_empty(), "{:?}", result.errors);
    assert!(result.bookmarks_pushed.is_empty());
    assert!(result.merge_requests.is_empty());
    assert!(output.contains("Would skip creating and pushing bookmarks"));
    assert!(output.contains("Would skip because pushing is disabled"));
    assert!(
        !output.contains("Would create"),
        "dry-run promises neither a bookmark push nor an MR: {output}"
    );
}
