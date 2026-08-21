//! Action-execute-layer regression tests for the disabled-push paths.
//!
//! These run in the CI gate (`--features no-e2e-tests`): they are plain
//! `#[tokio::test]` unit tests that construct the execute context in-process
//! (no forge, no network, no jj repo state), exercising the short-circuit
//! branches that fire *before* any jj subprocess.
//!
//! The load-bearing contract under test is the `pushed` flag on
//! `ActionResultData::Pushed`. Downstream, `execute.rs` does
//! `if pushed { bookmarks_pushed.extend(bookmarks) }`, so `pushed` decides
//! whether a bookmark shows up in the user-visible "pushed" summary. When
//! `jj-vine.push = false`, a `--dry-run --create` preview must NOT claim it
//! would push — otherwise the summary contradicts the `(pushing disabled)`
//! line the same run just printed.

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

/// Run a single action's `execute` against an in-process context with no live
/// forge and no jj repo state. The disabled / dry-run branches short-circuit
/// before any jj subprocess, so a bare tempdir cwd is enough.
async fn run_disabled_action<A: ExecuteAction>(
    action: A,
    dry_run: bool,
    push: RepoPushConfig,
) -> ActionResultData {
    let temp = TempDir::new().expect("temp dir");
    let jj = Jujutsu::new(temp.path()).expect("jj instance");
    let forge = ForgeImpl::Test(TestForge::default());
    let output = BufferedOutput::new();
    let config = Config::builder()
        .forge(ForgeType::Forgejo)
        .push(push)
        .build();
    // An empty graph: the disabled / dry-run branches never traverse it.
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
        no_hooks: false,
        plan: &plan,
    };
    let ctx = ExecuteActionContext {
        execute,
        current_results: Vec::new(),
    };

    action
        .execute(ctx)
        .await
        .expect("execute should succeed for a disabled/no-op push")
}

fn push_create_action() -> PushCreateAction {
    PushCreateAction {
        // >= 8 ASCII chars: execute slices `change_id[..8]` for display.
        change_ids: vec!["abcdefgh".to_owned()],
        remote: "origin".to_owned(),
    }
}

fn push_action() -> PushAction {
    PushAction {
        bookmarks: vec!["feature-a".to_owned()],
        remote: "origin".to_owned(),
    }
}

fn unwrap_pushed(result: ActionResultData) -> (Vec<String>, HashMap<String, String>, bool) {
    match result {
        ActionResultData::Pushed {
            bookmarks,
            created_bookmarks,
            pushed,
        } => (bookmarks, created_bookmarks, pushed),
        other => panic!("expected ActionResultData::Pushed, got {other:?}"),
    }
}

/// PRIMARY (RED against current code): a `--dry-run --create` submit with
/// pushing disabled must report `pushed == false`. Today `push_create.rs`
/// hardcodes `pushed: true` in the dry-run branch, so the preview lies: the
/// summary reports a push that a real run would never perform.
#[tokio::test]
async fn push_create_dry_run_disabled_reports_not_pushed() {
    let result =
        run_disabled_action(push_create_action(), true, RepoPushConfig::Enabled(false)).await;
    let (_bookmarks, _created, pushed) = unwrap_pushed(result);

    assert!(
        !pushed,
        "dry-run + `push = false` must report pushed == false; a preview that \
         claims it would push contradicts the `(pushing disabled)` plan line"
    );
}

/// SYMMETRY (RED against current code, green with the field-mirroring fix):
/// under dry-run + disabled push, no bookmarks may be reported. The downstream
/// `if pushed { bookmarks_pushed.extend(bookmarks) }` means these bookmarks
/// would surface in the "pushed" summary. The correct disabled path
/// (`push_create.rs` non-dry-run) returns empty collections + false; the
/// dry-run branch must match it.
#[tokio::test]
async fn push_create_dry_run_disabled_reports_no_bookmarks() {
    let result =
        run_disabled_action(push_create_action(), true, RepoPushConfig::Enabled(false)).await;
    let (bookmarks, created_bookmarks, _pushed) = unwrap_pushed(result);

    assert!(
        bookmarks.is_empty(),
        "dry-run + `push = false` must report no bookmarks as pushed, got {bookmarks:?}"
    );
    assert!(
        created_bookmarks.is_empty(),
        "dry-run + `push = false` must report no created bookmarks, got {created_bookmarks:?}"
    );
}

/// GUARD (green now, stays green): the real (non-dry-run) disabled create path
/// is already correct — it early-returns a no-op with empty collections and
/// `pushed == false`, attempting no push. This regression-guards that contract
/// so a future refactor can't silently start pushing (or reporting a push)
/// when `push = false`.
#[tokio::test]
async fn push_create_real_disabled_is_noop() {
    let result =
        run_disabled_action(push_create_action(), false, RepoPushConfig::Enabled(false)).await;
    let (bookmarks, created_bookmarks, pushed) = unwrap_pushed(result);

    assert!(!pushed, "non-dry-run + `push = false` must not push");
    assert!(
        bookmarks.is_empty(),
        "non-dry-run + `push = false` must report no bookmarks, got {bookmarks:?}"
    );
    assert!(
        created_bookmarks.is_empty(),
        "non-dry-run + `push = false` must report no created bookmarks, got {created_bookmarks:?}"
    );
}

/// GUARD (green now): `PushAction`'s dry-run disabled path already mirrors the
/// config correctly (`pushed: push_argv.is_some()` => false). This locks that
/// correct behavior against future drift toward the `push_create` defect.
#[tokio::test]
async fn push_dry_run_disabled_reports_not_pushed() {
    let result = run_disabled_action(push_action(), true, RepoPushConfig::Enabled(false)).await;
    let (_bookmarks, _created, pushed) = unwrap_pushed(result);

    assert!(
        !pushed,
        "dry-run + `push = false` must report pushed == false for PushAction"
    );
}

/// GUARD (green now): `PushAction`'s real disabled path is a no-op that reports
/// `pushed == false`, so nothing surfaces in the pushed summary.
#[tokio::test]
async fn push_real_disabled_is_noop() {
    let result = run_disabled_action(push_action(), false, RepoPushConfig::Enabled(false)).await;
    let (_bookmarks, _created, pushed) = unwrap_pushed(result);

    assert!(
        !pushed,
        "non-dry-run + `push = false` must report pushed == false for PushAction"
    );
}
