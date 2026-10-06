use std::{collections::HashSet, process::Command};

use assertables::{assert_contains, assert_not_contains};

use crate::{
    bookmark::{BookmarkGraph, BookmarkOrPending, change_id_to_temp_bookmark_name},
    commands::{
        GetBookmarksOptions,
        submit::{announce_submission_graph, select_changes_to_submit},
    },
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
async fn named_target_on_foreign_bookmark_fails_before_push() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    repo.set_config("jj-vine.forge", "github");
    repo.set_config("jj-vine.github.project", "owner/repo");
    repo.set_config("jj-vine.github.token", "gh-test-token");
    repo.set_config("jj-vine.fetch", "false");
    repo.set_config("user.email", "current@example.com");
    repo.set_config("user.name", "Current User");

    // main -> a (mine) -> c (coworker) -> b (mine)
    repo.jj.exec(["new", "main"])?;
    repo.create_change("a.txt", "a", "Change A")
        .create_bookmark("a")
        .create_bookmark("b")
        .push_bookmark("b");
    repo.jj.exec(["new"])?;
    repo.create_change("c.txt", "c", "Change C")
        .create_bookmark("c");
    repo.set_config("user.email", "author@example.com");
    repo.set_config("user.name", "Original Author");
    repo.jj.exec(["metaedit", "--update-author"])?;
    repo.set_config("user.email", "current@example.com");
    repo.set_config("user.name", "Current User");
    repo.jj.exec(["new"])?;
    repo.create_change("b.txt", "b", "Change B");
    repo.jj.exec(["bookmark", "set", "b", "--to", "@"])?;

    let remote_bookmark = || -> Result<String> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&repo.upstream().path)
            .args(["rev-parse", "refs/heads/b"])
            .output()?;
        assert!(output.status.success(), "remote bookmark b must exist");
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    };
    let remote_before = remote_bookmark()?;
    let local_target = repo
        .jj
        .log("b")?
        .first()
        .expect("local bookmark b exists")
        .commit_id
        .clone();
    assert_ne!(local_target, remote_before);

    let error = repo
        .try_run(["submit", "b"])
        .await
        .unwrap_err()
        .to_string();

    assert_contains!(error, "`b` stacks on `c`");
    assert_contains!(error, "jj-vine submit 'b | c'");
    let remote_after = remote_bookmark()?;
    assert_eq!(remote_after, remote_before);

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

    // main -> tracked A -> untracked foreign B -> tracked C
    repo.jj.exec(["new", "main"])?;
    repo.create_change("tracked-a.txt", "a", "Tracked ancestor")
        .create_and_push_bookmark("tracked-ancestor");
    repo.jj.exec(["new"])?;
    repo.create_change("untracked.txt", "untracked", "Untracked foreign ancestor")
        .create_bookmark("untracked-ancestor");
    repo.set_config("user.email", "author@example.com");
    repo.set_config("user.name", "Original Author");
    repo.jj.exec(["metaedit", "--update-author"])?;
    repo.set_config("user.email", "current@example.com");
    repo.set_config("user.name", "Current User");
    repo.jj.exec(["new"])?;
    repo.create_change("tracked-c.txt", "tracked", "Tracked descendant")
        .create_and_push_bookmark("tracked-descendant");

    let revset = GetBookmarksOptions::Tracked.to_revset();
    let changes = repo.jj.log(&revset)?;
    let bookmarks: Vec<_> = BookmarkOrPending::from_changes(&changes)
        .into_iter()
        .collect();
    let selected =
        select_changes_to_submit(&repo.jj, &revset, &bookmarks, &HashSet::new())?;
    let bookmark_graph = BookmarkGraph::from_changes(&repo.jj, &selected, true)?;
    let descendant = bookmark_graph
        .find_bookmark_in_components("tracked-descendant")
        .expect("tracked descendant is in the graph");
    assert_eq!(descendant.parent_name("main"), "tracked-ancestor");
    assert!(bookmark_graph.bookmark("untracked-ancestor").is_none());

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
