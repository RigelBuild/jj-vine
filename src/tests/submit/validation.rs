use assertables::assert_contains;

use crate::{error::Result, tests::TestRepo};

#[tokio::test]
async fn named_bookmark_in_trunk_errors_instead_of_silent_noop() -> Result<()> {
    let repo = TestRepo::with_local_remote();

    repo.set_config("jj-vine.forge", "github");
    repo.set_config("jj-vine.github.project", "owner/repo");
    repo.set_config("jj-vine.github.token", "gh-test-token");
    repo.jj
        .exec(["bookmark", "create", "on-trunk", "-r", "main"])?;

    let result = repo.try_run(["submit", "on-trunk", "--dry-run"]).await;

    assert_contains!(
        result.unwrap_err().to_string(),
        "found no changes to submit"
    );

    Ok(())
}
