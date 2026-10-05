use std::collections::HashSet;

use assertables::{assert_contains, assert_not_contains};

use crate::{
    bookmark::{BookmarkGraph, BookmarkOrPending, change_id_to_temp_bookmark_name},
    commands::submit::{announce_submission_graph, select_changes_to_submit},
    error::Result,
    output::BufferedOutput,
    tests::TestRepo,
};

#[tokio::test]
async fn named_bookmark_in_trunk_errors_instead_of_silent_noop() -> Result<()> {
    let repo = TestRepo::with_local_remote();

    repo.set_config("jj-vine.forge", "github");
    repo.set_config("jj-vine.github.project", "owner/repo");
    repo.set_config("jj-vine.github.token", "gh-test-token");
    repo.jj
        .exec(["bookmark", "create", "on-trunk", "-r", "main"])?;

    let error = repo
        .try_run(["submit", "on-trunk", "--dry-run"])
        .await
        .unwrap_err()
        .to_string();

    assert_contains!(error, "No changes to submit for bookmarks");

    Ok(())
}

#[tokio::test]
async fn tag_shadowed_bookmark_has_truthful_empty_selection_diagnostic() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    repo.set_config("jj-vine.forge", "github");
    repo.set_config("jj-vine.github.project", "owner/repo");
    repo.set_config("jj-vine.github.token", "gh-test-token");
    repo.set_config("user.email", "current@example.com");
    repo.set_config("user.name", "Current User");

    repo.jj.exec(["new", "main"])?;
    repo.create_change("shadowed.txt", "shadowed", "Shadowed change")
        .create_bookmark("shadowed");
    repo.jj.exec(["tag", "set", "shadowed", "-r", "@"])?;
    repo.set_config("user.email", "author@example.com");
    repo.set_config("user.name", "Original Author");
    repo.jj.exec(["metaedit", "--update-author"])?;

    let error = repo
        .try_run(["submit", "shadowed", "--dry-run"])
        .await
        .unwrap_err()
        .to_string();

    assert_contains!(error, "No changes to submit for bookmarks");

    Ok(())
}

#[tokio::test]
async fn generalized_foreign_target_is_not_announced() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    repo.set_config("jj-vine.forge", "github");
    repo.set_config("jj-vine.github.project", "owner/repo");
    repo.set_config("jj-vine.github.token", "gh-test-token");
    repo.set_config("user.email", "current@example.com");
    repo.set_config("user.name", "Current User");

    repo.jj.exec(["new", "main"])?;
    repo.create_change("own.txt", "own", "Own change")
        .create_bookmark("own-target");
    repo.jj.exec(["new", "main"])?;
    repo.create_change("foreign.txt", "foreign", "Foreign change")
        .create_bookmark("foreign-target");
    repo.set_config("user.email", "author@example.com");
    repo.set_config("user.name", "Original Author");
    repo.jj.exec(["metaedit", "--update-author"])?;
    repo.set_config("user.email", "current@example.com");
    repo.set_config("user.name", "Current User");

    let changes = repo.jj.log("bookmarks()")?;
    let bookmarks: Vec<_> = BookmarkOrPending::from_changes(&changes)
        .into_iter()
        .collect();
    let changes = select_changes_to_submit(&repo.jj, "bookmarks()", &bookmarks, &HashSet::new())?;
    let bookmark_graph = BookmarkGraph::from_changes(&repo.jj, &changes, false)?;
    let output = BufferedOutput::default();
    announce_submission_graph(&bookmark_graph, &bookmarks, true, false, &output)?;

    assert_contains!(output.get_buffer(), "Submitting bookmarks (dry run):");
    assert_contains!(output.get_buffer(), "own-target");
    assert_not_contains!(output.get_buffer(), "foreign-target");

    Ok(())
}

#[test]
fn tracked_announcement_omits_untracked_local_ancestor() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    repo.set_config("user.email", "current@example.com");
    repo.set_config("user.name", "Current User");

    repo.jj.exec(["new", "main"])?;
    repo.create_change("untracked.txt", "untracked", "Untracked ancestor")
        .create_bookmark("untracked-ancestor");
    repo.jj.exec(["new", "untracked-ancestor"])?;
    repo.create_change("tracked.txt", "tracked", "Tracked descendant")
        .create_and_push_bookmark("tracked-descendant");

    let changes = repo.jj.log("tracked-descendant")?;
    let bookmarks: Vec<_> = BookmarkOrPending::from_changes(&changes)
        .into_iter()
        .collect();
    assert!(
        bookmarks
            .iter()
            .any(|bookmark| bookmark.name() == "tracked-descendant")
    );
    let selected =
        select_changes_to_submit(&repo.jj, "tracked-descendant", &bookmarks, &HashSet::new())?;
    assert!(selected.iter().any(|change| {
        change
            .bookmarks
            .iter()
            .any(|bookmark| bookmark.name() == "untracked-ancestor")
    }));
    let bookmark_graph = BookmarkGraph::from_changes(&repo.jj, &selected, true)?;
    let output = BufferedOutput::default();
    announce_submission_graph(&bookmark_graph, &bookmarks, true, false, &output)?;

    assert_contains!(output.get_buffer(), "Submitting bookmarks (dry run):");
    assert_contains!(output.get_buffer(), "tracked-descendant");
    assert_not_contains!(output.get_buffer(), "untracked-ancestor");

    let no_changes: [&crate::jj::Change; 0] = [];
    let empty_graph = BookmarkGraph::from_changes(&repo.jj, no_changes, true)?;
    let empty_output = BufferedOutput::default();
    let error = announce_submission_graph(&empty_graph, &bookmarks, true, false, &empty_output)
        .unwrap_err()
        .to_string();

    assert_contains!(error, "No changes to submit for bookmarks");
    assert_not_contains!(empty_output.get_buffer(), "Submitting bookmarks");

    Ok(())
}

#[test]
fn submission_graph_announcement_includes_pending_bookmarks() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    repo.jj.exec(["new", "main"])?;
    repo.create_change("pending.txt", "pending", "Pending change");

    let change = repo.jj.log("@")?.pop().expect("working change exists");
    let pending = HashSet::from([change.change_id.clone()]);
    let changes = repo.jj.log_with_pending_bookmarks("@", &pending)?;
    let bookmarks: Vec<_> = BookmarkOrPending::from_changes(&changes)
        .into_iter()
        .collect();
    let selected = select_changes_to_submit(&repo.jj, "@", &bookmarks, &pending)?;
    let bookmark_graph = BookmarkGraph::from_changes(&repo.jj, &selected, false)?;
    let output = BufferedOutput::default();
    announce_submission_graph(&bookmark_graph, &bookmarks, true, false, &output)?;

    assert_contains!(
        output.get_buffer(),
        &change_id_to_temp_bookmark_name(&change.change_id)
    );

    Ok(())
}
