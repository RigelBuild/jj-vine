use assertables::assert_contains;

use crate::{
    description::{END_MARKER, START_MARKER},
    error::Result,
    forge::{Forge as _, UpdateMergeRequestInfoOptions},
    tests::TestRepo,
};

#[tokio::test]
async fn pr_description_includes_stack_info() -> Result<()> {
    let repo = TestRepo::with_github_remote();

    let branch_a = repo.bookmark_name("desc-a");
    let branch_b = repo.bookmark_name("desc-b");

    // main -> A -> B
    repo.jj.exec(["new", "main"])?;
    repo.create_change("a.txt", "a", "Commit A")
        .create_and_push_bookmark(&branch_a);

    repo.jj.exec(["new"])?;
    repo.create_change("b.txt", "b", "Commit B")
        .create_and_push_bookmark(&branch_b);

    repo.run(["submit", &branch_b]).await;

    let pr = repo
        .forge()
        .find_merge_request_by_source_branch(&branch_a)
        .await?
        .expect("PR A should exist");

    assert_contains!(pr.body.as_ref().unwrap(), START_MARKER);
    assert_contains!(pr.body.as_ref().unwrap(), END_MARKER);

    Ok(())
}

#[tokio::test]
async fn pr_description_links_to_dependent_prs() -> Result<()> {
    let repo = TestRepo::with_github_remote();

    let branch_a = repo.bookmark_name("link-a");
    let branch_b = repo.bookmark_name("link-b");

    // main -> A -> B
    repo.jj.exec(["new", "main"])?;
    repo.create_change("a.txt", "a", "Commit A")
        .create_and_push_bookmark(&branch_a);

    repo.jj.exec(["new"])?;
    repo.create_change("b.txt", "b", "Commit B")
        .create_and_push_bookmark(&branch_b);

    repo.run(["submit", &branch_b]).await;

    let a = repo
        .forge()
        .find_merge_request_by_source_branch(&branch_a)
        .await?
        .expect("PR A should exist");

    let b = repo
        .forge()
        .find_merge_request_by_source_branch(&branch_b)
        .await?
        .expect("PR B should exist");

    assert_contains!(a.body.unwrap(), &format!("#{}", b.number));
    assert_contains!(b.body.unwrap(), &format!("#{}", a.number));

    Ok(())
}

#[tokio::test]
async fn user_content_preserved_on_resubmit() -> Result<()> {
    let repo = TestRepo::with_github_remote();

    let branch_a = repo.bookmark_name("preserve-a");
    let branch_b = repo.bookmark_name("preserve-b");

    repo.jj.exec(["new", "main"])?;
    repo.create_change("a.txt", "a", "Commit A")
        .create_and_push_bookmark(&branch_a);

    repo.run(["submit", &branch_a]).await;

    let pr = repo
        .forge()
        .find_merge_request_by_source_branch(&branch_a)
        .await?
        .expect("PR A should exist");

    let user_content = "My important notes about this PR";
    repo.forge()
        .update_merge_request_info(
            pr.number,
            UpdateMergeRequestInfoOptions::builder()
                .description(user_content.to_owned())
                .current_is_draft(pr.draft)
                .current_title(pr.title.clone())
                .build(),
        )
        .await?;

    repo.jj.exec(["new"])?;
    repo.create_change("b.txt", "b", "Commit B")
        .create_and_push_bookmark(&branch_b);

    repo.run(["submit", &branch_b]).await;

    assert_contains!(
        repo.forge()
            .find_merge_request_by_source_branch(&branch_a)
            .await?
            .expect("PR A should exist")
            .body
            .unwrap(),
        user_content
    );

    Ok(())
}

#[tokio::test]
async fn add_markers_to_description_without_markers() -> Result<()> {
    let repo = TestRepo::with_github_remote();

    let branch_a = repo.bookmark_name("markers-a");
    let branch_b = repo.bookmark_name("markers-b");

    // main -> A
    repo.jj.exec(["new", "main"])?;
    repo.create_change("a.txt", "a", "Commit A")
        .create_and_push_bookmark(&branch_a);

    repo.run(["submit", &branch_a]).await;

    let pr = repo
        .forge()
        .find_merge_request_by_source_branch(&branch_a)
        .await?
        .expect("PR A should exist");

    let user_description = "Custom description without markers";
    repo.forge()
        .update_merge_request_info(
            pr.number,
            UpdateMergeRequestInfoOptions::builder()
                .description(user_description.to_owned())
                .current_is_draft(pr.draft)
                .current_title(pr.title.clone())
                .build(),
        )
        .await?;

    repo.jj.exec(["new"])?;
    repo.create_change("b.txt", "b", "Commit B")
        .create_and_push_bookmark(&branch_b);

    repo.run(["submit", &branch_b]).await;

    let pr = repo
        .forge()
        .find_merge_request_by_source_branch(&branch_a)
        .await?
        .expect("PR A should exist");

    assert_contains!(pr.body.as_ref().unwrap(), START_MARKER);
    assert_contains!(pr.body.as_ref().unwrap(), END_MARKER);
    assert_contains!(pr.body.as_ref().unwrap(), user_description);

    Ok(())
}

#[tokio::test]
async fn moving_stack_to_top_preserves_user_text_and_is_stable() -> Result<()> {
    let repo = TestRepo::with_github_remote();

    let branch_a = repo.bookmark_name("placement-a");
    let branch_b = repo.bookmark_name("placement-b");

    // main -> A -> B
    repo.jj.exec(["new", "main"])?;
    repo.create_change("a.txt", "a", "Commit A")
        .create_and_push_bookmark(&branch_a);

    repo.jj.exec(["new"])?;
    repo.create_change("b.txt", "b", "Commit B")
        .create_and_push_bookmark(&branch_b);

    repo.run(["submit", &branch_b]).await;

    let pr_a = repo
        .forge()
        .find_merge_request_by_source_branch(&branch_a)
        .await?
        .expect("PR A should exist");
    let pr_b = repo
        .forge()
        .find_merge_request_by_source_branch(&branch_b)
        .await?
        .expect("PR B should exist");

    let user_before = "Context before the stack details";
    let user_after = "Follow-up details after the stack";
    let initial_description = pr_a.body.expect("PR A should have a description");
    assert!(initial_description.ends_with(END_MARKER));
    let edited_description = format!("{user_before}\n\n{initial_description}\n\n{user_after}");
    repo.forge()
        .update_merge_request_info(
            pr_a.number,
            UpdateMergeRequestInfoOptions::builder()
                .description(edited_description)
                .current_is_draft(pr_a.draft)
                .current_title(pr_a.title.clone())
                .build(),
        )
        .await?;

    repo.jj.exec([
        "config",
        "set",
        "--repo",
        "jj-vine.description.placement",
        "top",
    ])?;
    repo.run(["submit", &branch_b]).await;

    let updated_description = repo
        .forge()
        .find_merge_request_by_source_branch(&branch_a)
        .await?
        .expect("PR A should exist")
        .body
        .expect("PR A should have a description");

    assert!(updated_description.starts_with(START_MARKER));
    assert_eq!(updated_description.matches(START_MARKER).count(), 1);
    assert_eq!(updated_description.matches(END_MARKER).count(), 1);
    let stack_end = updated_description
        .find(END_MARKER)
        .expect("stack end marker should exist");
    let stack = &updated_description[..stack_end];
    assert_contains!(stack, "This PR is part of a stack containing 2 PRs");
    assert_contains!(stack, "this PR");
    assert_contains!(stack, &format!("#{}", pr_b.number));
    assert_eq!(updated_description.matches(user_before).count(), 1);
    assert_eq!(updated_description.matches(user_after).count(), 1);
    let stack_end_position = updated_description
        .find(END_MARKER)
        .expect("stack end marker should exist");
    let user_before_position = updated_description
        .find(user_before)
        .expect("the first user text should be preserved");
    let user_after_position = updated_description
        .find(user_after)
        .expect("the second user text should be preserved");
    assert!(stack_end_position < user_before_position);
    assert!(user_before_position < user_after_position);

    repo.run(["submit", &branch_b]).await;

    assert_eq!(
        updated_description,
        repo.forge()
            .find_merge_request_by_source_branch(&branch_a)
            .await?
            .expect("PR A should exist")
            .body
            .expect("PR A should have a description")
    );

    Ok(())
}

#[tokio::test]
async fn skip_update_when_description_unchanged() -> Result<()> {
    let repo = TestRepo::with_github_remote();

    let branch_a = repo.bookmark_name("unchanged-a");
    let branch_b = repo.bookmark_name("unchanged-b");

    // main -> A -> B
    repo.jj.exec(["new", "main"])?;
    repo.create_change("a.txt", "a", "Commit A")
        .create_and_push_bookmark(&branch_a);

    repo.jj.exec(["new"])?;
    repo.create_change("b.txt", "b", "Commit B")
        .create_and_push_bookmark(&branch_b);

    repo.run(["submit", &branch_b]).await;

    let initial_desc = repo
        .forge()
        .find_merge_request_by_source_branch(&branch_a)
        .await?
        .expect("PR A should exist")
        .body
        .unwrap()
        .clone();

    repo.run(["submit", &branch_b]).await;

    assert_eq!(
        initial_desc,
        repo.forge()
            .find_merge_request_by_source_branch(&branch_a)
            .await?
            .expect("PR A should exist")
            .body
            .unwrap()
    );

    Ok(())
}
