use std::collections::HashSet;

use assertables::{assert_contains, assert_not_contains};

use crate::{
    bookmark::BookmarkOrPending,
    commands::submit::select_changes_to_submit_and_announce,
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
    let output = BufferedOutput::default();
    select_changes_to_submit_and_announce(
        &repo.jj,
        "bookmarks()",
        &bookmarks,
        &HashSet::new(),
        true,
        false,
        &output,
    )?;

    assert_contains!(output.get_buffer(), "Submitting bookmarks (dry run):");
    assert_contains!(output.get_buffer(), "own-target");
    assert_not_contains!(output.get_buffer(), "foreign-target");

    Ok(())
}
