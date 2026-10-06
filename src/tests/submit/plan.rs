use std::collections::{HashMap, HashSet};

use crate::{
    bookmark::{BookmarkGraph, BookmarkOrPending},
    config::{Config, DescriptionConfig, ForgeType, RepoPushConfig, TitleConfig},
    forge::{
        Forge as _,
        ForgeImpl,
        MergeRequestLike as _,
        test::{MergeRequest, TestForge},
    },
    output::BufferedOutput,
    submit::{
        PlanContext,
        RootExecuteContext,
        execute::{self, Action, BookmarkNameOrPendingChangeId},
        find_changes_to_submit,
        plan,
    },
    tests::TestRepo,
};

#[tokio::test]
async fn github_root_pr_base_is_preserved_while_child_base_is_repaired() {
    let repo = TestRepo::with_local_remote();
    let root = repo.bookmark_name("root");
    let child = repo.bookmark_name("child");
    repo.create_change("root.txt", "root", "Root commit")
        .create_and_push_bookmark(&root);
    repo.new_on(&root)
        .create_change("child.txt", "child", "Child commit")
        .create_and_push_bookmark(&child);
    let fresh_root = repo.bookmark_name("fresh-root");
    repo.new_on("main")
        .create_change("fresh-root.txt", "fresh root", "Fresh root commit")
        .create_and_push_bookmark(&fresh_root);
    let new_root = repo.bookmark_name("new-root");
    repo.new_on("main")
        .create_change("new-root.txt", "new root", "New root commit")
        .create_and_push_bookmark(&new_root);
    repo.new_on("main");

    let forge = ForgeImpl::Test(
        TestForge::builder()
            .merge_requests(HashMap::from([
                (
                    "101".to_owned(),
                    MergeRequest::builder()
                        .id("101".to_owned())
                        .title("Root PR".to_owned())
                        .source_branch(root.clone())
                        .target_branch("rigel-release".to_owned())
                        .build(),
                ),
                (
                    "102".to_owned(),
                    MergeRequest::builder()
                        .id("102".to_owned())
                        .title("Child PR".to_owned())
                        .source_branch(child.clone())
                        .target_branch("main".to_owned())
                        .build(),
                ),
                (
                    "103".to_owned(),
                    MergeRequest::builder()
                        .id("103".to_owned())
                        .title("Fresh root PR".to_owned())
                        .source_branch(fresh_root.clone())
                        .target_branch("main".to_owned())
                        .build(),
                ),
            ]))
            .build(),
    );
    let config = Config::builder()
        .forge(ForgeType::GitHub)
        .push(RepoPushConfig::Enabled(false))
        .title(TitleConfig {
            sync_single_revision: false,
            sync_multiple_revisions: false,
            ..TitleConfig::default()
        })
        .description(DescriptionConfig {
            enabled: false,
            ..DescriptionConfig::default()
        })
        .build();
    let output = BufferedOutput::new();
    let pending = HashSet::new();
    let targets = repo
        .jj
        .log_with_pending_bookmarks(&format!("{child} | {fresh_root} | {new_root}"), &pending)
        .expect("read submit target");
    let bookmarks: Vec<_> = BookmarkOrPending::from_changes(&targets)
        .into_iter()
        .collect();
    let changes = find_changes_to_submit(
        &repo.jj,
        bookmarks.iter().map(BookmarkOrPending::change_id),
        &pending,
    )
    .expect("find changes to submit");
    let graph =
        BookmarkGraph::from_changes(&repo.jj, &changes, false).expect("build bookmark graph");
    let submission_plan = plan::plan(PlanContext {
        jj: &repo.jj,
        forge: &forge,
        config: &config,
        output: &output,
        bookmark_graph: &graph,
        dry_run: false,
    })
    .await
    .expect("plan submit");
    assert_eq!(submission_plan.existing_mrs.len(), 3);
    assert!(
        !submission_plan.actions.iter().flatten().any(
            |action| matches!(action, Action::UpdateMRBase(update) if update.bookmark == root)
        ),
        "a GitHub root PR with a manual target must not get a base-update action"
    );
    assert!(
        !submission_plan.actions.iter().flatten().any(
            |action| matches!(action, Action::UpdateMRBase(update) if update.bookmark == fresh_root)
        ),
        "an existing root PR with the current default base needs no update"
    );
    assert!(
        submission_plan.actions.iter().flatten().any(
            |action| matches!(action, Action::UpdateMRBase(update) if update.bookmark == child && update.new_target_branch == root)
        ),
        "an existing child PR must still be retargeted to its parent branch"
    );
    assert!(
        submission_plan.actions.iter().flatten().any(
            |action| matches!(action, Action::CreateMR(create) if create.bookmark == BookmarkNameOrPendingChangeId::Bookmark(new_root.clone()) && create.target_branch == "main")
        ),
        "a new GitHub root PR must keep normal default-base selection"
    );

    let result = execute::execute(RootExecuteContext::new(
        &repo.jj,
        &forge,
        &config,
        &output,
        false,
        submission_plan,
        changes,
        false,
        false,
    ))
    .await
    .expect("execute submit");
    assert!(result.errors.is_empty(), "{:?}", result.errors);

    let root_pr = forge
        .find_merge_request_by_source_branch(&root)
        .await
        .expect("query root PR")
        .expect("root PR exists");
    let child_pr = forge
        .find_merge_request_by_source_branch(&child)
        .await
        .expect("query child PR")
        .expect("child PR exists");
    assert_eq!(root_pr.target_branch(), "rigel-release");
    assert_eq!(child_pr.target_branch(), root);
    let new_root_pr = forge
        .find_merge_request_by_source_branch(&new_root)
        .await
        .expect("query new root PR")
        .expect("new root PR exists");
    assert_eq!(new_root_pr.target_branch(), "main");
}
