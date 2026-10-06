use crate::{
    bookmark::BookmarkGraph,
    config::{Config, ForgeType},
    error::Result,
    forge::{
        ForgeImpl,
        test::{MergeRequest, TestForge},
    },
    output::BufferedOutput,
    submit::{PlanContext, execute::Action, plan::plan},
    tests::TestRepo,
};

fn with_main_trunk() -> TestRepo<()> {
    let repo = TestRepo::with_main();
    repo.set_config(r#"revset-aliases."trunk()""#, "main");
    repo
}

fn config(default_base_branch: Option<&str>) -> Config {
    Config::builder()
        .forge(ForgeType::GitHub)
        .maybe_default_base_branch(default_base_branch.map(ToOwned::to_owned))
        .build()
}

async fn plan_for(
    repo: &TestRepo<()>,
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

#[tokio::test]
async fn planner_creates_root_mr_with_configured_base_and_keeps_child_parent_base() -> Result<()> {
    let repo = with_main_trunk();
    let root = repo.bookmark_name("root");
    let child = repo.bookmark_name("child");
    repo.create_change_and_tracked_bookmark(&root)
        .jj(["new"])?
        .create_change_and_tracked_bookmark(&child);

    let config = config(Some("release"));
    let forge = ForgeImpl::Test(TestForge::default());
    let changes = repo.jj.log("mine() & bookmarks()")?;
    let graph = BookmarkGraph::from_changes(&repo.jj, &changes, false)?;
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
async fn planner_retargets_existing_root_mr_from_trunk_to_configured_base() -> Result<()> {
    let repo = with_main_trunk();
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
    let graph = BookmarkGraph::from_changes(&repo.jj, &changes, false)?;
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
    let repo = with_main_trunk();
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
    let graph = BookmarkGraph::from_changes(&repo.jj, &changes, false)?;
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
    let repo = with_main_trunk();
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
    let graph = BookmarkGraph::from_changes(&repo.jj, &changes, false)?;
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
async fn planner_keeps_trunk_target_when_root_base_is_unset() -> Result<()> {
    let repo = with_main_trunk();
    let root = repo.bookmark_name("root-default");
    repo.create_change_and_tracked_bookmark(&root);

    let config = config(None);
    let forge = ForgeImpl::Test(TestForge::default());
    let changes = repo.jj.log("mine() & bookmarks()")?;
    let graph = BookmarkGraph::from_changes(&repo.jj, &changes, false)?;
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
