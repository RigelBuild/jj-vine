use std::collections::HashSet;

use assertables::{
    assert_any,
    assert_contains,
    assert_is_empty,
    assert_none,
    assert_not_contains,
    assert_some,
};

use crate::{
    bookmark::{Bookmark, BookmarkGraph, BookmarkOrPending, BookmarkRef},
    commands::submit::select_changes_to_submit,
    error::Result,
    submit::find_changes_to_submit,
    tests::TestRepo,
};

#[test]
fn deleted_middle_bookmark() -> Result<()> {
    let repo = TestRepo::new();

    // a -> b -> c
    repo.commit_with_bookmark("file1.txt", "content1", "Commit A", "bookmark-a")
        .commit_with_bookmark("file2.txt", "content2", "Commit B", "bookmark-b")
        .commit_with_bookmark("file3.txt", "content3", "Commit C", "bookmark-c");

    repo.jj.exec(["bookmark", "delete", "bookmark-b"]).unwrap();

    let changes = repo.jj.log("mine() & bookmarks()")?;
    let bookmarks: Vec<_> = BookmarkOrPending::from_changes(&changes)
        .into_iter()
        .collect();

    let graph = BookmarkGraph::from_bookmarks(&repo.jj, bookmarks.iter().cloned(), false)?;

    let bookmark_a = graph.find_bookmark_in_components("bookmark-a").unwrap();

    assert_none!(graph.find_bookmark_in_components("bookmark-b"));

    let bookmark_c = graph.find_bookmark_in_components("bookmark-c").unwrap();

    assert_any!(bookmark_c.parents.iter(), |p| p
        == &BookmarkRef::Bookmark(bookmark_a.clone()));

    Ok(())
}

#[test]
fn base_branch_not_included_in_submission() -> Result<()> {
    let repo = TestRepo::with_local_remote();

    repo.create_change("f1.txt", "feature1", "Feature 1")
        .create_bookmark("feature-1");

    repo.jj.exec(["new"]).unwrap();
    repo.create_change("f2.txt", "feature2", "Feature 2")
        .create_bookmark("feature-2");

    let changes = repo.jj.log("::feature-2")?;
    let bookmarks: Vec<_> = BookmarkOrPending::from_changes(&changes)
        .into_iter()
        .collect();

    let graph = BookmarkGraph::from_bookmarks(&repo.jj, bookmarks.iter().cloned(), false)?;
    let stack = graph.component_containing("feature-2").unwrap();

    assert_contains!(stack, "feature-1");
    assert_contains!(stack, "feature-2");
    assert_not_contains!(stack, "main");

    Ok(())
}

#[test]
fn submit_base_branch_errors() -> Result<()> {
    let repo = TestRepo::with_local_remote();

    repo.create_change("feature.txt", "feature", "Feature commit")
        .create_bookmark("feature-1");

    let changes = repo.jj.log("main")?;
    let bookmarks: Vec<_> = BookmarkOrPending::from_changes(&changes)
        .into_iter()
        .collect();
    let graph = BookmarkGraph::from_bookmarks(&repo.jj, bookmarks.iter().cloned(), true)?;

    assert_is_empty!(graph.components());

    Ok(())
}

#[test]
fn graph_skips_default_branch_history() -> Result<()> {
    let repo = TestRepo::with_local_remote();

    for i in 1_u32..=50_u32 {
        repo.create_change(
            &format!("file{i}.txt"),
            &format!("content {i}"),
            &format!("Commit {i}"),
        );
        repo.jj.exec(["commit", "-m", &format!("Commit {i}")])?;
    }

    repo.jj.exec(["bookmark", "set", "main", "--to", "@-"])?;
    repo.jj.exec(["git", "push"])?;

    repo.jj.exec(["new", "main"])?;
    repo.create_change("feature.txt", "feature", "Feature commit")
        .create_bookmark("feature-1");

    let changes = repo.jj.log("mine() & bookmarks()")?;
    let bookmarks: Vec<_> = BookmarkOrPending::from_changes(&changes)
        .into_iter()
        .collect();

    let graph = BookmarkGraph::from_bookmarks(&repo.jj, bookmarks.iter().cloned(), false)?;

    assert_none!(graph.component_containing("main"));
    assert_some!(graph.component_containing("feature-1"));

    Ok(())
}

#[test]
fn find_changes_to_submit_with_advanced_main() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    repo.create_change("f1.txt", "f1", "Feature 1")
        .create_bookmark("feature");

    repo.jj.exec(["new", "main"])?;
    repo.create_change("m1.txt", "m1", "Main advance");
    repo.jj.exec(["bookmark", "set", "main", "--to", "@"])?;
    repo.jj.exec(["git", "push"])?;

    repo.jj.exec(["new"])?;
    repo.create_change("f2.txt", "f2", "Feature 2")
        .create_bookmark("feature-2");

    repo.jj.exec(["new"])?;
    repo.create_change("f3.txt", "f3", "Feature 3")
        .create_bookmark("feature-3");

    // old-main --> main --> feature-2 --> feature-3
    //   /-->feature

    // From feature-3, we expect the whole downstack back to (but not including)
    // trunk — i.e. feature-2 and feature-3.
    let changes = find_changes_to_submit(&repo.jj, ["feature-3"], ["feature-3"], &HashSet::new())?;
    let mut names: Vec<_> = Bookmark::from_changes(&changes)
        .into_iter()
        .map(|b| b.name().to_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["feature-2".to_owned(), "feature-3".to_owned()]);

    // From feature, we expect just feature: it branched off old main but is
    // not in the ancestry of the new trunk, so it should not be filtered out.
    let changes = find_changes_to_submit(&repo.jj, ["feature"], ["feature"], &HashSet::new())?;
    let names: Vec<_> = Bookmark::from_changes(&changes)
        .into_iter()
        .map(|b| b.name().to_owned())
        .collect();
    assert_eq!(names, vec!["feature".to_owned()]);

    Ok(())
}

#[test]
fn find_changes_to_submit_includes_foreign_authored_named_target() -> Result<()> {
    let repo = TestRepo::with_local_remote();

    repo.set_config("user.email", "current@example.com");
    repo.set_config("user.name", "Current User");
    repo.jj.exec(["new", "main"])?;
    repo.create_change("feature.txt", "feature", "Feature commit")
        .create_bookmark("feature");
    repo.set_config("user.email", "author@example.com");
    repo.set_config("user.name", "Original Author");
    repo.jj.exec(["metaedit", "--update-author"])?;
    repo.set_config("user.email", "current@example.com");
    repo.set_config("user.name", "Current User");
    // The explicitly named bookmark is included even though its commit author
    // differs from the current user.
    let changes = find_changes_to_submit(&repo.jj, ["feature"], ["feature"], &HashSet::new())?;
    let names: Vec<_> = Bookmark::from_changes(&changes)
        .into_iter()
        .map(|bookmark| bookmark.name().to_owned())
        .collect();
    assert_eq!(names, vec!["feature".to_owned()]);

    Ok(())
}

#[test]
fn find_changes_to_submit_excludes_foreign_authored_ancestry_companion() -> Result<()> {
    let repo = TestRepo::with_local_remote();

    repo.set_config("user.email", "current@example.com");
    repo.set_config("user.name", "Current User");
    repo.jj.exec(["new", "main"])?;
    repo.create_change("a.txt", "a", "Change A")
        .create_bookmark("a");

    // Build a stack where a foreign-authored bookmark sits between two changes
    // authored by the current user.
    repo.jj.exec(["new"])?;
    repo.create_change("c.txt", "c", "Change C")
        .create_bookmark("c");
    repo.set_config("user.email", "author@example.com");
    repo.set_config("user.name", "Original Author");
    repo.jj.exec(["metaedit", "--update-author"])?;
    repo.set_config("user.email", "current@example.com");
    repo.set_config("user.name", "Current User");

    repo.jj.exec(["new"])?;
    repo.create_change("b.txt", "b", "Change B")
        .create_bookmark("b");

    // Submitting the top bookmark walks the ancestry, but must omit the foreign
    // middle bookmark.
    let changes = find_changes_to_submit(&repo.jj, ["b"], ["b"], &HashSet::new())?;
    let mut names: Vec<_> = Bookmark::from_changes(&changes)
        .into_iter()
        .map(|bookmark| bookmark.name().to_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["a".to_owned(), "b".to_owned()]);

    // A separate foreign-authored target must survive the multi-target union.
    repo.jj.exec(["new", "main"])?;
    repo.create_change("d.txt", "d", "Change D")
        .create_bookmark("d");
    repo.set_config("user.email", "author@example.com");
    repo.set_config("user.name", "Original Author");
    repo.jj.exec(["metaedit", "--update-author"])?;
    repo.set_config("user.email", "current@example.com");
    repo.set_config("user.name", "Current User");

    let changes = find_changes_to_submit(&repo.jj, ["b", "d"], ["b", "d"], &HashSet::new())?;
    let mut names: Vec<_> = Bookmark::from_changes(&changes)
        .into_iter()
        .map(|bookmark| bookmark.name().to_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["a".to_owned(), "b".to_owned(), "d".to_owned()]);

    Ok(())
}

#[test]
fn find_changes_to_submit_includes_pending_bookmark_without_local_bookmark() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    repo.jj.exec(["new", "main"])?;
    repo.create_change("pending.txt", "pending", "Pending change");
    let change = repo.jj.log("@")?.pop().expect("working change exists");
    let pending = HashSet::from([change.change_id.clone()]);

    let changes = find_changes_to_submit(
        &repo.jj,
        [change.change_id.as_str()],
        [] as [&str; 0],
        &pending,
    )?;
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].change_id, change.change_id);
    assert!(changes[0].pending_bookmark);
    assert!(changes[0].bookmarks.is_empty());

    Ok(())
}

/// Re-author the working-copy change as a different user, then restore the
/// current user so `mine()` no longer matches it.
fn make_foreign(repo: &TestRepo<TestRepo<()>>) -> Result<()> {
    repo.set_config("user.email", "author@example.com");
    repo.set_config("user.name", "Original Author");
    repo.jj.exec(["metaedit", "--update-author"])?;
    repo.set_config("user.email", "current@example.com");
    repo.set_config("user.name", "Current User");
    Ok(())
}

fn as_current_user(repo: &TestRepo<TestRepo<()>>) {
    repo.set_config("user.email", "current@example.com");
    repo.set_config("user.name", "Current User");
}

/// Runs the production selection in `commands::submit::submit`: resolve the
/// revset, then select the changes to submit from its bookmarks.
fn submission_changes(
    repo: &TestRepo<TestRepo<()>>,
    revset: &str,
) -> Result<Vec<crate::jj::Change>> {
    let changes = repo.jj.log(revset)?;
    let bookmarks: Vec<_> = BookmarkOrPending::from_changes(&changes)
        .into_iter()
        .collect();
    select_changes_to_submit(&repo.jj, revset, &bookmarks, &HashSet::new())
}

fn sorted_names(changes: &[crate::jj::Change]) -> Vec<String> {
    let mut names: Vec<_> = Bookmark::from_changes(changes)
        .into_iter()
        .map(|bookmark| bookmark.name().to_owned())
        .collect();
    names.sort();
    names
}

#[test]
fn graph_skips_foreign_parent_of_named_target() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    as_current_user(&repo);

    // main -> x (foreign) -> y (mine)
    repo.jj.exec(["new", "main"])?;
    repo.create_change("x.txt", "x", "Change X")
        .create_bookmark("x");
    make_foreign(&repo)?;
    repo.jj.exec(["new"])?;
    repo.create_change("y.txt", "y", "Change Y")
        .create_bookmark("y");

    let changes = submission_changes(&repo, "y")?;
    assert_eq!(sorted_names(&changes), vec!["y".to_owned()]);

    let graph = BookmarkGraph::from_changes(&repo.jj, &changes, false)?;
    let y = graph.find_bookmark_in_components("y").unwrap();
    assert_eq!(y.parents, vec![]);
    assert_none!(graph.find_bookmark_in_components("x"));

    Ok(())
}

#[test]
fn graph_walks_past_foreign_middle_bookmark() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    as_current_user(&repo);

    // main -> a (mine) -> c (foreign) -> b (mine)
    repo.jj.exec(["new", "main"])?;
    repo.create_change("a.txt", "a", "Change A")
        .create_bookmark("a");
    repo.jj.exec(["new"])?;
    repo.create_change("c.txt", "c", "Change C")
        .create_bookmark("c");
    make_foreign(&repo)?;
    repo.jj.exec(["new"])?;
    repo.create_change("b.txt", "b", "Change B")
        .create_bookmark("b");

    let changes = submission_changes(&repo, "b")?;
    assert_eq!(sorted_names(&changes), vec!["a".to_owned(), "b".to_owned()]);

    let graph = BookmarkGraph::from_changes(&repo.jj, &changes, false)?;
    let a = graph.find_bookmark_in_components("a").unwrap();
    let b = graph.find_bookmark_in_components("b").unwrap();
    assert_eq!(b.parents, vec![BookmarkRef::Bookmark(a.clone())]);
    assert_none!(graph.find_bookmark_in_components("c"));

    Ok(())
}

#[test]
fn general_revset_keeps_mine_filter_for_foreign_targets() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    as_current_user(&repo);

    // main -> mine-1 ; main -> foreign-1
    repo.jj.exec(["new", "main"])?;
    repo.create_change("m.txt", "m", "Mine")
        .create_bookmark("mine-1");
    repo.jj.exec(["new", "main"])?;
    repo.create_change("f.txt", "f", "Foreign")
        .create_bookmark("foreign-1");
    make_foreign(&repo)?;

    // Only a literally named bookmark bypasses `mine()`; `bookmarks()` does not.
    let changes = submission_changes(&repo, "bookmarks()")?;
    assert_eq!(sorted_names(&changes), vec!["mine-1".to_owned()]);

    let graph = BookmarkGraph::from_changes(&repo.jj, &changes, false)?;
    assert_some!(graph.component_containing("mine-1"));
    assert_none!(graph.component_containing("foreign-1"));

    Ok(())
}

#[test]
fn named_foreign_bookmark_builds_graph() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    as_current_user(&repo);

    repo.jj.exec(["new", "main"])?;
    repo.create_change("f.txt", "f", "Foreign")
        .create_bookmark("foreign-1");
    make_foreign(&repo)?;

    for revset in ["foreign-1", "\"foreign-1\""] {
        let changes = submission_changes(&repo, revset)?;
        assert_eq!(sorted_names(&changes), vec!["foreign-1".to_owned()]);

        let graph = BookmarkGraph::from_changes(&repo.jj, &changes, false)?;
        let target = graph.find_bookmark_in_components("foreign-1").unwrap();
        assert_eq!(target.parents, vec![]);
    }

    Ok(())
}

#[test]
fn literal_spellings_of_foreign_bookmarks_bypass_mine() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    as_current_user(&repo);

    // main -> "a|b" (foreign) ; main -> foreign-2 (foreign) ; main -> x(y)
    // (foreign)
    repo.jj.exec(["new", "main"])?;
    repo.create_change("p.txt", "p", "Pipe")
        .create_bookmark("a|b");
    make_foreign(&repo)?;
    repo.jj.exec(["new", "main"])?;
    repo.create_change("f.txt", "f", "Foreign")
        .create_bookmark("foreign-2");
    make_foreign(&repo)?;
    repo.jj.exec(["new", "main"])?;
    repo.create_change("x.txt", "x", "Parens")
        .create_bookmark("x(y)");
    make_foreign(&repo)?;

    let cases: [(&str, &[&str]); 7] = [
        (r#""a|b""#, &["a|b"]),
        ("'a|b'", &["a|b"]),
        ("(foreign-2)", &["foreign-2"]),
        ("'foreign-2'", &["foreign-2"]),
        (r#"( "a|b" | (foreign-2) )"#, &["a|b", "foreign-2"]),
        (r#""x(y)" | foreign-2"#, &["foreign-2", "x(y)"]),
        (r#"foreign-2 | bookmarks(exact:"a|b")"#, &["foreign-2"]),
    ];
    for (revset, expected) in cases {
        let changes = submission_changes(&repo, revset)?;
        assert_eq!(sorted_names(&changes), expected, "revset {revset}");
    }

    Ok(())
}

#[test]
fn named_bypass_excludes_coworker_bookmark_on_same_change() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    as_current_user(&repo);

    // main -> shared (foreign), carrying both `named` and `coworker`.
    repo.jj.exec(["new", "main"])?;
    repo.create_change("s.txt", "s", "Shared")
        .create_bookmark("named")
        .create_bookmark("coworker");
    make_foreign(&repo)?;

    let changes = submission_changes(&repo, "named")?;
    assert_eq!(sorted_names(&changes), vec!["named".to_owned()]);

    let graph = BookmarkGraph::from_changes(&repo.jj, &changes, false)?;
    assert_some!(graph.find_bookmark_in_components("named"));
    assert_none!(graph.find_bookmark_in_components("coworker"));

    Ok(())
}

#[test]
fn generalized_selectors_of_foreign_bookmark_keep_mine_filter() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    as_current_user(&repo);

    repo.jj.exec(["new", "main"])?;
    repo.create_change("f.txt", "f", "Foreign")
        .create_bookmark("foreign-1");
    make_foreign(&repo)?;

    // Each selector resolves to `foreign-1` without naming it as a union member.
    for revset in [
        r#"bookmarks(exact:"foreign-1")"#,
        "present(foreign-1)",
        "foreign-1 & bookmarks()",
        "(foreign-1 & bookmarks()) | none()",
        "foreign-1 ~ none()",
        "foreign-1+-",
    ] {
        let changes = submission_changes(&repo, revset)?;
        assert_is_empty!(changes, "revset {revset}");
    }

    Ok(())
}

#[test]
fn dropped_generalized_target_does_not_seed_ancestry() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    as_current_user(&repo);

    // main -> wip-x (mine) -> team-cw (foreign)
    repo.jj.exec(["new", "main"])?;
    repo.create_change("w.txt", "w", "Wip")
        .create_bookmark("wip-x");
    repo.jj.exec(["new"])?;
    repo.create_change("t.txt", "t", "Team")
        .create_bookmark("team-cw");
    make_foreign(&repo)?;

    // The selector resolves only to the foreign `team-cw`, which `mine()`
    // drops; its own ancestor `wip-x` was never selected.
    let changes = submission_changes(&repo, r#"bookmarks(glob:"team-*")"#)?;
    assert_is_empty!(changes);

    // Naming the foreign bookmark still submits it with the user's ancestors.
    let changes = submission_changes(&repo, "team-cw")?;
    assert_eq!(
        sorted_names(&changes),
        vec!["team-cw".to_owned(), "wip-x".to_owned()]
    );

    Ok(())
}

#[test]
fn alias_or_tag_named_like_foreign_bookmark_keeps_mine_filter() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    as_current_user(&repo);

    // main -> tagged (mine, tag `stack`) ; main -> stack (foreign) ; main ->
    // work (foreign)
    repo.jj.exec(["new", "main"])?;
    repo.create_change("t.txt", "t", "Tagged");
    repo.jj.exec(["tag", "set", "stack", "-r", "@"])?;
    repo.jj.exec(["new", "main"])?;
    repo.create_change("s.txt", "s", "Stack")
        .create_bookmark("stack");
    make_foreign(&repo)?;
    repo.jj.exec(["new", "main"])?;
    repo.create_change("w.txt", "w", "Work")
        .create_bookmark("work");
    make_foreign(&repo)?;
    repo.set_config("revset-aliases.work", r#"bookmarks(glob:"wor*")"#);

    // The symbol resolves to the tag or expands the alias, so the foreign
    // bookmark is selected only by a generalized expression.
    for revset in [
        r#"stack | bookmarks(exact:"stack")"#,
        r#""stack" | bookmarks(exact:"stack")"#,
        "work",
        "(work)",
    ] {
        let changes = submission_changes(&repo, revset)?;
        assert_is_empty!(changes, "revset {revset}");
    }

    // A string literal is never alias-expanded, so it still names the bookmark.
    for revset in [r#""work""#, "'work'"] {
        let changes = submission_changes(&repo, revset)?;
        assert_eq!(
            sorted_names(&changes),
            vec!["work".to_owned()],
            "revset {revset}"
        );
    }

    Ok(())
}

#[test]
fn hex_escaped_non_ascii_name_bypasses_mine() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    as_current_user(&repo);

    repo.jj.exec(["new", "main"])?;
    repo.create_change("e.txt", "e", "Accent")
        .create_bookmark("é");
    make_foreign(&repo)?;

    // jj decodes `\xHH` to the character U+00HH.
    let changes = submission_changes(&repo, r#""\xe9""#)?;
    assert_eq!(sorted_names(&changes), vec!["é".to_owned()]);

    Ok(())
}

#[test]
fn decomposed_foreign_bookmark_bypasses_mine_only_when_named_bare() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    as_current_user(&repo);

    let decomposed_name = "cafe\u{301}";
    repo.jj.exec(["new", "main"])?;
    repo.create_change("e.txt", "e", "Decomposed name")
        .create_bookmark(decomposed_name);
    make_foreign(&repo)?;

    let changes = submission_changes(&repo, decomposed_name)?;
    assert_eq!(sorted_names(&changes), vec![decomposed_name.to_owned()]);

    let changes = submission_changes(&repo, &format!("present({decomposed_name})"))?;
    assert_is_empty!(changes, "generalized revset for {decomposed_name}");

    Ok(())
}

#[cfg(not(feature = "no-e2e-tests"))]
mod e2e {
    use assertables::assert_contains;

    use crate::{error::Result, tests::TestRepo};

    #[tokio::test]
    async fn multiple_independent_stacks_dont_incorrectly_retarget() -> Result<()> {
        let repo = TestRepo::with_forgejo_remote();

        // main -> a -> b
        // main -> c

        repo.jj.exec(["new", "main"])?;
        let a = repo.bookmark_name("a");
        repo.create_change("file1.txt", "a content", "A")
            .create_and_push_bookmark(&a);

        repo.jj.exec(["new"])?;
        let b = repo.bookmark_name("b");
        repo.create_change("file2.txt", "b content", "B")
            .create_and_push_bookmark(&b);

        repo.jj.exec(["new", "main"])?;
        let c = repo.bookmark_name("c");
        repo.create_change("file3.txt", "c content", "C")
            .create_and_push_bookmark(&c);

        let output = repo.run(["submit", "--tracked", "--dry-run"]).await;

        assert_contains!(output, &format!("Would create PR {a} -> main"));
        assert_contains!(output, &format!("Would create PR {b} -> {a}"));
        assert_contains!(output, &format!("Would create PR {c} -> main"));

        Ok(())
    }

    #[tokio::test]
    async fn complex_bookmark_name() -> Result<()> {
        let repo = TestRepo::with_forgejo_remote();

        let name = repo.bookmark_name("complex-bookmark--parent/complex--name");

        repo.create_change_and_bookmark(&name);

        let output = repo
            .run(["submit", &format!("\"{name}\""), "--dry-run"])
            .await;

        assert_contains!(output, &format!("Would create PR {name} -> main"));

        Ok(())
    }

    #[tokio::test]
    async fn complex_bookmark_name_tracked() -> Result<()> {
        let repo = TestRepo::with_forgejo_remote();

        let name = repo.bookmark_name("complex-bookmark--parent/complex--name");

        repo.create_change_and_tracked_bookmark(&name)
            .push_bookmark(&name);

        let output = repo.run(["submit", "--tracked", "--dry-run"]).await;

        assert_contains!(output, &format!("Would create PR {name} -> main"));

        Ok(())
    }
}
