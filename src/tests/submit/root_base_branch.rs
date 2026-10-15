use crate::{
    bookmark::BookmarkGraph,
    config::{Config, ForgeType, TitleConfig, TitleFormat},
    error::Result,
    forge::{
        ForgeImpl,
        test::{MergeRequest, TestForge},
    },
    output::BufferedOutput,
    submit::{PlanContext, execute::Action, plan::plan},
    tests::TestRepo,
};

fn with_main_trunk() -> Result<TestRepo<TestRepo<()>>> {
    let repo = TestRepo::with_local_remote();
    repo.set_config(r#"revset-aliases."trunk()""#, "main@origin");
    repo.jj.exec(["new", "main"])?;
    repo.create_change("release.txt", "release", "Commit A on configured base")
        .create_bookmark("release")
        .push_bookmark("release");
    repo.jj.exec(["git", "fetch"])?;
    repo.jj.exec(["bookmark", "delete", "release"])?;
    repo.jj.exec(["new", "release@origin"])?;
    Ok(repo)
}

fn with_release_behind_trunk() -> Result<(TestRepo<TestRepo<()>>, String)> {
    let repo = TestRepo::with_local_remote();
    repo.set_config(r#"revset-aliases."trunk()""#, "main@origin");
    repo.jj.exec(["new", "main@origin"])?;
    repo.create_change("release.txt", "release", "Commit A on release")
        .create_bookmark("release")
        .push_bookmark("release")
        .track_bookmark("release");
    repo.jj
        .exec(["bookmark", "set", "main", "--to", "release"])?;
    repo.jj.exec(["new", "main"])?;
    let trunk_title = "Commit B on trunk only";
    repo.create_change("trunk.txt", "trunk", trunk_title)
        .jj(["bookmark", "set", "main", "--to", "@"])?
        .push_bookmark("main");

    repo.jj.exec(["git", "fetch"])?;
    repo.jj.exec(["new", "main@origin"])?;

    let root = repo.bookmark_name("root-trunk-commits");
    repo.create_change(
        "feature.txt",
        "feature",
        "Commit C feature title\nFeature details",
    )
    .create_bookmark(&root);
    Ok((repo, root))
}

fn config(default_base_branch: Option<&str>) -> Config {
    Config::builder()
        .forge(ForgeType::GitHub)
        .maybe_default_base_branch(default_base_branch.map(ToOwned::to_owned))
        .build()
}

async fn plan_for<T>(
    repo: &TestRepo<T>,
    config: &Config,
    forge: &ForgeImpl,
    graph: &BookmarkGraph<'_>,
) -> Result<crate::submit::plan::SubmissionPlan> {
    let output = BufferedOutput::new();
    plan(PlanContext {
        jj: &repo.jj,
        forge,
        config,
        output: &output,
        bookmark_graph: graph,
        dry_run: false,
    })
    .await
}

fn with_merged_release_ancestor() -> Result<TestRepo<TestRepo<()>>> {
    let repo = with_main_trunk()?;
    repo.create_change("merged.txt", "merged", "Already merged")
        .create_bookmark("merged")
        .push_bookmark("merged")
        .track_bookmark("merged");
    repo.jj
        .exec(["bookmark", "set", "release", "--to", "merged"])?;
    repo.push_bookmark("release");
    repo.jj.exec(["git", "fetch"])?;
    repo.jj.exec(["new", "release@origin"])?;
    repo.create_change("feature.txt", "feature", "Feature")
        .create_bookmark("feature");
    Ok(repo)
}

#[test]
fn find_changes_to_submit_excludes_bookmark_merged_into_configured_base() -> Result<()> {
    let repo = with_merged_release_ancestor()?;
    let merged_boundary_revset = config(Some("release")).merged_boundary_revset();
    let changes = crate::submit::find_changes_to_submit(
        &repo.jj,
        ["feature"],
        ["feature"],
        &std::collections::HashSet::new(),
        &merged_boundary_revset,
    )?;

    assert!(
        changes
            .iter()
            .flat_map(|change| &change.bookmarks)
            .all(|bookmark| bookmark.name() != "merged"),
        "the merged bookmark must not be selected for submission"
    );
    Ok(())
}

fn selected_bookmarks(
    repo: &TestRepo<TestRepo<()>>,
    target: &str,
    boundary: &str,
) -> Result<Vec<String>> {
    Ok(crate::submit::find_changes_to_submit(
        &repo.jj,
        [target],
        [target],
        &std::collections::HashSet::new(),
        boundary,
    )?
    .iter()
    .flat_map(|change| change.bookmarks.iter().map(|b| b.name().to_owned()))
    .collect())
}

#[test]
fn explicitly_named_bookmark_merged_into_configured_base_is_not_selected() -> Result<()> {
    let repo = with_merged_release_ancestor()?;
    let boundary = config(Some("release")).merged_boundary_revset();

    assert_eq!(
        selected_bookmarks(&repo, "merged", &boundary)?,
        Vec::<String>::new()
    );
    Ok(())
}

#[test]
fn configured_base_behind_trunk_still_excludes_trunk_ancestry() -> Result<()> {
    let (repo, root) = with_release_behind_trunk()?;
    let boundary = config(Some("release")).merged_boundary_revset();

    assert_eq!(selected_bookmarks(&repo, &root, &boundary)?, vec![root]);
    Ok(())
}

#[test]
fn unfetched_configured_base_falls_back_to_trunk() -> Result<()> {
    let repo = with_merged_release_ancestor()?;
    let boundary = config(Some("not-on-remote")).merged_boundary_revset();

    let selected = selected_bookmarks(&repo, "feature", &boundary)?;
    assert_eq!(selected, selected_bookmarks(&repo, "feature", "trunk()")?);
    assert!(selected.contains(&"merged".to_owned()));
    Ok(())
}

#[test]
fn default_selectors_exclude_bookmarks_merged_into_configured_base() -> Result<()> {
    let repo = with_merged_release_ancestor()?;
    repo.push_bookmark("feature").track_bookmark("feature");
    repo.jj.exec(["git", "fetch"])?;
    let boundary = config(Some("release")).merged_boundary_revset();

    for (label, option) in [
        ("mine", crate::commands::GetBookmarksOptions::Mine),
        ("tracked", crate::commands::GetBookmarksOptions::Tracked),
    ] {
        let names: Vec<String> = repo
            .jj
            .log(option.to_revset(&boundary))?
            .iter()
            .flat_map(|change| change.bookmarks.iter().map(|b| b.name().to_owned()))
            .collect();
        assert!(names.contains(&"feature".to_owned()), "{label}: {names:?}");
        assert!(!names.contains(&"merged".to_owned()), "{label}: {names:?}");
    }
    Ok(())
}

#[tokio::test]
async fn planner_uses_configured_base_as_root_for_release_ancestor() -> Result<()> {
    let repo = with_merged_release_ancestor()?;
    let config = config(Some("release"));
    let merged_boundary_revset = config.merged_boundary_revset();
    let all_changes = repo.jj.log("mine() & bookmarks()")?;
    let graph =
        BookmarkGraph::from_changes(&repo.jj, &all_changes, false, &merged_boundary_revset)?;
    assert!(graph.bookmark("merged").is_none());
    let feature = graph
        .find_bookmark_in_components("feature")
        .expect("feature is in the submission graph");
    assert!(feature.parents.is_empty(), "feature is a stack root");

    let forge = ForgeImpl::Test(TestForge::default());
    let plan = plan_for(&repo, &config, &forge, &graph).await?;
    assert!(plan.actions.iter().flatten().any(|action| matches!(
        action,
        Action::CreateMR(action)
            if action.bookmark.to_string() == "feature" && action.target_branch == "release"
    )));
    assert!(!plan.actions.iter().flatten().any(|action| matches!(
        action,
        Action::CreateMR(action) if action.bookmark.to_string() == "merged"
    )));
    assert!(!plan.actions.iter().flatten().any(|action| matches!(
        action,
        Action::UpdateMRBase(action) if action.bookmark.to_string() == "merged"
    )));
    Ok(())
}

#[tokio::test]
async fn planner_creates_root_mr_with_configured_base_and_keeps_child_parent_base() -> Result<()> {
    let repo = with_main_trunk()?;
    let root = repo.bookmark_name("root");
    let child = repo.bookmark_name("child");
    repo.create_change_and_tracked_bookmark(&root)
        .jj(["new"])?
        .create_change_and_tracked_bookmark(&child);

    let config = config(Some("release"));
    let forge = ForgeImpl::Test(TestForge::default());
    let changes = repo.jj.log("mine() & bookmarks()")?;
    let graph =
        BookmarkGraph::from_changes(&repo.jj, &changes, false, &config.merged_boundary_revset())?;
    let plan = plan_for(&repo, &config, &forge, &graph).await?;
    let create_targets: std::collections::HashMap<_, _> = plan
        .actions
        .iter()
        .flatten()
        .filter_map(|action| match action {
            Action::CreateMR(action) => {
                Some((action.bookmark.to_string(), action.target_branch.clone()))
            }
            _ => None,
        })
        .collect();

    assert_eq!(
        create_targets.get(&root).map(String::as_str),
        Some("release")
    );
    assert_eq!(
        create_targets.get(&child).map(String::as_str),
        Some(root.as_str())
    );
    Ok(())
}

#[tokio::test]
async fn planner_titles_root_mr_from_configured_remote_base() -> Result<()> {
    let repo = with_main_trunk()?;
    let root = repo.bookmark_name("root-configured-base-title");
    repo.create_change("feature.txt", "feature", "Commit B title\nDetails")
        .create_bookmark(&root);
    let config = Config::builder()
        .forge(ForgeType::GitHub)
        .maybe_default_base_branch(Some("release".to_owned()))
        .remote_name("origin".to_owned())
        .title(TitleConfig {
            multiple_revisions: TitleFormat::FirstRevisionFirstLine,
            ..Default::default()
        })
        .build();
    let forge = ForgeImpl::Test(TestForge::default());
    let changes = repo.jj.log("mine() & bookmarks()")?;
    let graph =
        BookmarkGraph::from_changes(&repo.jj, &changes, false, &config.merged_boundary_revset())?;
    let plan = plan_for(&repo, &config, &forge, &graph).await?;

    let title = plan
        .actions
        .iter()
        .flatten()
        .find_map(|action| match action {
            Action::CreateMR(action) if action.bookmark.to_string() == root => {
                Some(action.title.as_str())
            }
            _ => None,
        });
    assert_eq!(title, Some("Commit B title"));

    let create_description = plan
        .actions
        .iter()
        .flatten()
        .find_map(|action| match action {
            Action::CreateMR(action) if action.bookmark.to_string() == root => {
                Some(action.description.as_str())
            }
            _ => None,
        });
    assert_eq!(create_description, Some("Details"));
    Ok(())
}

#[tokio::test]
async fn planner_uses_configured_base_without_dropping_trunk_commits() -> Result<()> {
    let (repo, root) = with_release_behind_trunk()?;
    let config = Config::builder()
        .forge(ForgeType::GitHub)
        .maybe_default_base_branch(Some("release".to_owned()))
        .title(TitleConfig {
            multiple_revisions: TitleFormat::FirstRevisionFirstLine,
            ..Default::default()
        })
        .build();
    let forge = ForgeImpl::Test(TestForge::default());
    let changes = repo.jj.log("mine() & bookmarks()")?;
    let graph =
        BookmarkGraph::from_changes(&repo.jj, &changes, false, &config.merged_boundary_revset())?;
    let plan = plan_for(&repo, &config, &forge, &graph).await?;

    let create_mr = plan
        .actions
        .iter()
        .flatten()
        .find_map(|action| match action {
            Action::CreateMR(action) if action.bookmark.to_string() == root => Some(action),
            _ => None,
        })
        .expect("root MR is created");
    assert_eq!(create_mr.title, "Commit B on trunk only");
    assert!(create_mr.description.contains("Commit B on trunk only"));
    assert!(create_mr.description.contains("Commit C feature title"));
    assert!(!create_mr.description.contains("Commit A on release"));
    Ok(())
}

#[tokio::test]
async fn planner_syncs_existing_root_mr_title_and_description_from_trunk_only_commit() -> Result<()>
{
    let (repo, root) = with_release_behind_trunk()?;
    let config = Config::builder()
        .forge(ForgeType::GitHub)
        .maybe_default_base_branch(Some("release".to_owned()))
        .title(TitleConfig {
            sync_multiple_revisions: true,
            multiple_revisions: TitleFormat::FirstRevisionFirstLine,
            ..Default::default()
        })
        .description(crate::config::DescriptionConfig {
            sync: true,
            ..Default::default()
        })
        .build();
    let forge = ForgeImpl::Test(
        TestForge::builder()
            .merge_requests(std::collections::HashMap::from([(
                "1".to_owned(),
                MergeRequest::builder()
                    .id("1".to_owned())
                    .title("Old title".to_owned())
                    .source_branch(root.clone())
                    .target_branch("release".to_owned())
                    .build(),
            )]))
            .build(),
    );
    let changes = repo.jj.log("mine() & bookmarks()")?;
    let graph =
        BookmarkGraph::from_changes(&repo.jj, &changes, false, &config.merged_boundary_revset())?;
    let plan = plan_for(&repo, &config, &forge, &graph).await?;

    let synced_action = plan
        .actions
        .iter()
        .flatten()
        .find_map(|action| match action {
            Action::UpdateMRTitleDescription(action) if action.bookmark.to_string() == root => {
                Some(action)
            }
            _ => None,
        })
        .expect("root MR title and description sync");
    assert_eq!(
        synced_action.title.as_deref(),
        Some("Commit B on trunk only")
    );
    let description = synced_action.description.as_deref().unwrap_or_default();
    assert!(description.contains("Commit B on trunk only"));
    assert!(description.contains("Commit C feature title"));
    assert!(!description.contains("Commit A on release"));
    Ok(())
}

#[tokio::test]
async fn planner_uses_configured_base_when_trunk_is_unavailable() -> Result<()> {
    let repo = TestRepo::new();
    let root = repo.bookmark_name("root-without-trunk");
    repo.create_change_and_tracked_bookmark(&root);

    let config = config(Some("release"));
    let forge = ForgeImpl::Test(TestForge::default());
    let changes = repo.jj.log("mine() & bookmarks()")?;
    let graph =
        BookmarkGraph::from_changes(&repo.jj, &changes, false, &config.merged_boundary_revset())?;
    let plan = plan_for(&repo, &config, &forge, &graph).await?;

    let target = plan
        .actions
        .iter()
        .flatten()
        .find_map(|action| match action {
            Action::CreateMR(action) if action.bookmark.to_string() == root => {
                Some(action.target_branch.as_str())
            }
            _ => None,
        });
    assert_eq!(target, Some("release"));
    Ok(())
}

#[tokio::test]
async fn planner_retargets_existing_fork_child_mr_from_trunk_to_configured_base() -> Result<()> {
    let repo = with_main_trunk()?;
    let root = repo.bookmark_name("fork-root");
    let child = repo.bookmark_name("fork-child");
    repo.create_change_and_tracked_bookmark(&root)
        .jj(["new"])?
        .create_change_and_tracked_bookmark(&child);
    let forge = ForgeImpl::Test(
        TestForge::builder()
            .source_project_id("fork/project".to_owned())
            .target_project_id("upstream/project".to_owned())
            .merge_requests(std::collections::HashMap::from([(
                "1".to_owned(),
                MergeRequest::builder()
                    .id("1".to_owned())
                    .title("Fork child".to_owned())
                    .source_branch(child.clone())
                    .target_branch("main".to_owned())
                    .build(),
            )]))
            .build(),
    );
    let config = config(Some("release"));
    let changes = repo.jj.log("mine() & bookmarks()")?;
    let graph =
        BookmarkGraph::from_changes(&repo.jj, &changes, false, &config.merged_boundary_revset())?;
    let plan = plan_for(&repo, &config, &forge, &graph).await?;

    assert!(plan.actions.iter().flatten().any(|action| matches!(
        action,
        Action::UpdateMRBase(action)
            if action.bookmark == child && action.new_target_branch == "release"
    )));
    assert!(!plan.actions.iter().flatten().any(|action| matches!(
        action,
        Action::CreateMR(action) if action.bookmark.to_string() == child
    )));
    Ok(())
}

#[tokio::test]
async fn planner_retargets_existing_root_mr_from_trunk_to_configured_base() -> Result<()> {
    let repo = with_main_trunk()?;
    let root = repo.bookmark_name("root-retarget");
    repo.create_change_and_tracked_bookmark(&root);

    let forge = ForgeImpl::Test(
        TestForge::builder()
            .merge_requests(std::collections::HashMap::from([(
                "1".to_owned(),
                MergeRequest::builder()
                    .id("1".to_owned())
                    .title("Root".to_owned())
                    .source_branch(root.clone())
                    .target_branch("main".to_owned())
                    .build(),
            )]))
            .build(),
    );
    let config = config(Some("release"));
    let changes = repo.jj.log("mine() & bookmarks()")?;
    let graph =
        BookmarkGraph::from_changes(&repo.jj, &changes, false, &config.merged_boundary_revset())?;
    let plan = plan_for(&repo, &config, &forge, &graph).await?;
    let update = plan
        .actions
        .iter()
        .flatten()
        .find_map(|action| match action {
            Action::UpdateMRBase(action) => Some(action),
            _ => None,
        });

    assert_eq!(
        update.map(|action| action.bookmark.as_str()),
        Some(root.as_str())
    );
    assert_eq!(
        update.map(|action| action.new_target_branch.as_str()),
        Some("release")
    );
    Ok(())
}

#[tokio::test]
async fn planner_prefers_existing_root_mr_on_configured_base() -> Result<()> {
    let repo = with_main_trunk()?;
    let root = repo.bookmark_name("root-configured-base");
    repo.create_change_and_tracked_bookmark(&root);

    let forge = ForgeImpl::Test(
        TestForge::builder()
            .merge_requests(std::collections::HashMap::from([
                (
                    "1".to_owned(),
                    MergeRequest::builder()
                        .id("1".to_owned())
                        .title("Root on main".to_owned())
                        .source_branch(root.clone())
                        .target_branch("main".to_owned())
                        .build(),
                ),
                (
                    "2".to_owned(),
                    MergeRequest::builder()
                        .id("2".to_owned())
                        .title("Root on release".to_owned())
                        .source_branch(root.clone())
                        .target_branch("release".to_owned())
                        .build(),
                ),
            ]))
            .build(),
    );
    let config = config(Some("release"));
    let changes = repo.jj.log("mine() & bookmarks()")?;
    let graph =
        BookmarkGraph::from_changes(&repo.jj, &changes, false, &config.merged_boundary_revset())?;
    let plan = plan_for(&repo, &config, &forge, &graph).await?;

    assert_eq!(plan.existing_mrs[&root].target_branch(), "release");
    assert!(
        !plan
            .actions
            .iter()
            .flatten()
            .any(|action| matches!(action, Action::UpdateMRBase(_)))
    );
    Ok(())
}

#[tokio::test]
async fn planner_keeps_existing_child_mr_target_at_parent_bookmark() -> Result<()> {
    let repo = with_main_trunk()?;
    let root = repo.bookmark_name("root-child-target");
    let child = repo.bookmark_name("child-target");
    repo.create_change_and_tracked_bookmark(&root)
        .jj(["new"])?
        .create_change_and_tracked_bookmark(&child);

    let forge = ForgeImpl::Test(
        TestForge::builder()
            .merge_requests(std::collections::HashMap::from([
                (
                    "1".to_owned(),
                    MergeRequest::builder()
                        .id("1".to_owned())
                        .title("Root".to_owned())
                        .source_branch(root.clone())
                        .target_branch("release".to_owned())
                        .build(),
                ),
                (
                    "2".to_owned(),
                    MergeRequest::builder()
                        .id("2".to_owned())
                        .source_branch(child.clone())
                        .title("Child".to_owned())
                        .target_branch(root.clone())
                        .build(),
                ),
            ]))
            .build(),
    );
    let config = config(Some("release"));
    let changes = repo.jj.log("mine() & bookmarks()")?;
    let graph =
        BookmarkGraph::from_changes(&repo.jj, &changes, false, &config.merged_boundary_revset())?;
    let plan = plan_for(&repo, &config, &forge, &graph).await?;
    let child_base_updates: Vec<_> = plan
        .actions
        .iter()
        .flatten()
        .filter_map(|action| match action {
            Action::UpdateMRBase(action) if action.bookmark == child => Some(action),
            _ => None,
        })
        .collect();

    assert!(child_base_updates.is_empty());
    assert_eq!(plan.existing_mrs[&child].target_branch(), root);
    Ok(())
}

#[tokio::test]
async fn planner_retargets_existing_child_mr_from_deleted_middle_base() -> Result<()> {
    let repo = with_main_trunk()?;
    let root = repo.bookmark_name("root-deleted-middle");
    let child = repo.bookmark_name("child-deleted-middle");
    repo.create_change_and_tracked_bookmark(&root)
        .jj(["new"])?
        .create_change_and_tracked_bookmark(&child);

    let forge = ForgeImpl::Test(
        TestForge::builder()
            .merge_requests(std::collections::HashMap::from([
                (
                    "1".to_owned(),
                    MergeRequest::builder()
                        .id("1".to_owned())
                        .title("Root".to_owned())
                        .source_branch(root.clone())
                        .target_branch("main".to_owned())
                        .build(),
                ),
                (
                    "2".to_owned(),
                    MergeRequest::builder()
                        .id("2".to_owned())
                        .title("Child".to_owned())
                        .source_branch(child.clone())
                        .target_branch("deleted-middle".to_owned())
                        .build(),
                ),
            ]))
            .build(),
    );
    let config = config(None);
    let changes = repo.jj.log("mine() & bookmarks()")?;
    let graph =
        BookmarkGraph::from_changes(&repo.jj, &changes, false, &config.merged_boundary_revset())?;
    let plan = plan_for(&repo, &config, &forge, &graph).await?;

    assert!(plan.actions.iter().flatten().any(|action| matches!(
        action,
        Action::UpdateMRBase(action)
            if action.bookmark == child && action.new_target_branch == root
    )));
    assert!(!plan.actions.iter().flatten().any(|action| matches!(
        action,
        Action::CreateMR(action) if action.bookmark.to_string() == child
    )));
    Ok(())
}

#[tokio::test]
async fn planner_keeps_trunk_target_when_root_base_is_unset() -> Result<()> {
    let repo = with_main_trunk()?;
    let root = repo.bookmark_name("root-default");
    repo.create_change_and_tracked_bookmark(&root);

    let config = config(None);
    let forge = ForgeImpl::Test(TestForge::default());
    let changes = repo.jj.log("mine() & bookmarks()")?;
    let graph =
        BookmarkGraph::from_changes(&repo.jj, &changes, false, &config.merged_boundary_revset())?;
    let plan = plan_for(&repo, &config, &forge, &graph).await?;
    let target = plan
        .actions
        .iter()
        .flatten()
        .find_map(|action| match action {
            Action::CreateMR(action) if action.bookmark.to_string() == root => {
                Some(action.target_branch.as_str())
            }
            _ => None,
        });

    assert_eq!(target, Some("main"));
    Ok(())
}
