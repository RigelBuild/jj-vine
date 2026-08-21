use assertables::assert_contains;

use crate::{error::Result, tests::TestRepo};

#[cfg(not(feature = "no-e2e-tests"))]
mod e2e {
    use assertables::assert_contains;

    use crate::{error::Result, tests::TestRepo};

    #[tokio::test]
    async fn bookmark_and_tracked_mutually_exclusive() -> Result<()> {
        let repo = TestRepo::new();

        let result = repo.try_run(["submit", "some-bookmark", "--tracked"]).await;

        assert_contains!(result.unwrap_err().to_string(), "Usage:");

        Ok(())
    }

    #[tokio::test]
    async fn submit_requires_bookmark_or_tracked() -> Result<()> {
        let repo = TestRepo::new();

        let result = repo.try_run(["submit"]).await;

        assert_contains!(result.unwrap_err().to_string(), "Usage:");

        Ok(())
    }

    #[tokio::test]
    async fn tracked_with_no_pushed_bookmarks() -> Result<()> {
        let repo = TestRepo::new();

        repo.jj.exec(["new"])?;
        repo.create_change("test.txt", "content", "Test commit")
            .create_bookmark("unpushed-branch");

        let result = repo.try_run(["submit", "--tracked"]).await;

        assert_contains!(
            result.unwrap_err().to_string(),
            "No bookmarks in revset (mine() & tracked_remote_bookmarks()) ~ trunk()"
        );

        Ok(())
    }
}

#[tokio::test]
async fn named_bookmark_in_trunk_errors_instead_of_silent_noop() -> Result<()> {
    let repo = TestRepo::with_local_remote();

    // A bookmark pointing at a commit already in trunk resolves in pass 1
    // (verbatim, no mine() filter) but has nothing to submit in pass 2. It must
    // error, not exit 0 with "No bookmarks pushed" (RIG-2267 backstop). The
    // guard runs before any forge network call, so a config that merely passes
    // validation plus --dry-run keeps this off the network and lets it gate CI
    // (which runs with --features no-e2e-tests). Seed the full minimum GitHub
    // config (forge + project + token) at the repo layer — `with_local_remote`
    // sets no jj-vine config, and relying on an ambient user-level `forge`/token
    // would make this test pass only on a developer box (the exact config-leak
    // non-hermeticity documented in config.rs), while CI's clean HOME fails the
    // parse with "missing field `forge`" before ever reaching the guard.
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
