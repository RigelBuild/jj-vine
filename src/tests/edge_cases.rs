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
    let changes = find_changes_to_submit(&repo.jj, ["feature-3"], &HashSet::new())?;
    let mut names: Vec<_> = Bookmark::from_changes(&changes)
        .into_iter()
        .map(|b| b.name().to_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["feature-2".to_owned(), "feature-3".to_owned()]);

    // From feature, we expect just feature: it branched off old main but is
    // not in the ancestry of the new trunk, so it should not be filtered out.
    let changes = find_changes_to_submit(&repo.jj, ["feature"], &HashSet::new())?;
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

    repo.jj.exec(["new", "main"])?;
    repo.create_change("f1.txt", "f1", "Feature 1")
        .create_bookmark("feature");

    // Author the bookmarked commit under a *different* identity than the one
    // configured now — the RIG-2267 trigger (the fleet commit-author flip left
    // pre-flip commits authored under the old identity). `mine()` would drop it.
    repo.set_config("user.email", "seal@sealedsecurity.com");
    repo.set_config("user.name", "seal");
    repo.jj.exec(["metaedit", "--update-author"])?;
    repo.set_config("user.email", "mintaka@rigel.build");
    repo.set_config("user.name", "mintaka");

    // An explicitly-named target must be submitted regardless of its author.
    let changes = find_changes_to_submit(&repo.jj, ["feature"], &HashSet::new())?;
    let names: Vec<_> = Bookmark::from_changes(&changes)
        .into_iter()
        .map(|b| b.name().to_owned())
        .collect();
    assert_eq!(names, vec!["feature".to_owned()]);

    Ok(())
}

#[test]
fn find_changes_to_submit_excludes_foreign_authored_ancestry_companion() -> Result<()> {
    let repo = TestRepo::with_local_remote();

    // Build a stack off trunk: a (mine) -> c (foreign) -> b (mine), where the
    // middle bookmark `c` is authored under a *different* identity (the
    // RIG-2267 commit-author flip). Only the explicitly-named target is taken
    // raw; ancestry-walked companions stay narrowed to `mine()`, so a foreign
    // companion sitting in the ancestry of the target must be EXCLUDED — the
    // other half of the fix (a stacked submit must not sweep in other people's
    // bookmarks). Without `& mine()` on the ancestry branch, `c` would leak in.
    repo.set_config("user.email", "mintaka@rigel.build");
    repo.set_config("user.name", "mintaka");

    repo.jj.exec(["new", "main"])?;
    repo.create_change("a.txt", "a", "Change A")
        .create_bookmark("a");

    repo.jj.exec(["new"])?;
    repo.create_change("c.txt", "c", "Change C")
        .create_bookmark("c");
    // Re-author `c` (=@) under the old identity, then restore `user.email` so
    // `mine()` resolves to `mintaka` at query time.
    repo.set_config("user.email", "seal@sealedsecurity.com");
    repo.set_config("user.name", "seal");
    repo.jj.exec(["metaedit", "--update-author"])?;
    repo.set_config("user.email", "mintaka@rigel.build");
    repo.set_config("user.name", "mintaka");

    repo.jj.exec(["new"])?;
    repo.create_change("b.txt", "b", "Change B")
        .create_bookmark("b");

    // Submitting `b` walks its ancestry: `a` (mine) is included, `c` (foreign)
    // is dropped by `& mine()`.
    let changes = find_changes_to_submit(&repo.jj, ["b"], &HashSet::new())?;
    let mut names: Vec<_> = Bookmark::from_changes(&changes)
        .into_iter()
        .map(|b| b.name().to_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["a".to_owned(), "b".to_owned()]);

    // Two explicit targets exercise the multi-target `join(" | ")` on both the
    // explicit and ancestry branches; `c` stays excluded from the ancestry.
    let changes = find_changes_to_submit(&repo.jj, ["a", "b"], &HashSet::new())?;
    let mut names: Vec<_> = Bookmark::from_changes(&changes)
        .into_iter()
        .map(|b| b.name().to_owned())
        .collect();
    names.sort();
    assert_eq!(names, vec!["a".to_owned(), "b".to_owned()]);

    Ok(())
}

/// Regression: a history with chained merge commits must not re-walk shared
/// ancestors combinatorially. The old `find_nearest_bookmarked_ancestors`
/// recursed with no visited set and spawned one `jj log` per visit, so a ladder
/// of merges produced an exponential number of `jj` invocations and did not
/// terminate on a moderately branchy repo. With the visited set the walk is
/// linear: the invocation count stays a small multiple of the commit count.
#[test]
fn merge_ladder_does_not_rewalk_combinatorially() -> Result<()> {
    let repo = TestRepo::new();

    // Build a ladder of diamonds on top of a single bookmarked `base`. Each
    // rung merges two children of the previous rung, so the previous rung is a
    // shared ancestor reachable by two paths. Under the old recursion each rung
    // doubled how many times the lower rungs were re-expanded (2^depth); the
    // visited set collapses that to one visit each.
    //
    // The interior rungs must carry no bookmark, or the walk stops at the first
    // one. We use temporary bookmarks only to navigate while building, then
    // delete them, leaving `base` and `leaf` as the only bookmarks — so
    // resolving `leaf` descends the whole ladder to `base`.
    repo.create_change("base.txt", "base", "base")
        .create_bookmark("base");

    // 8 rungs: without the fix this spawns 514 jj invocations (2.5x the 200
    // ceiling), so it separates the two regimes decisively at a third less build
    // cost than a deeper ladder.
    let rungs: u32 = 8;
    for i in 0..rungs {
        let parent = if i == 0 {
            "base".to_owned()
        } else {
            format!("rung{}", i - 1)
        };
        repo.jj(["new", &parent])?
            .create_change(&format!("l{i}.txt"), "l", "left")
            .jj(["new", &parent])?
            .create_change(&format!("r{i}.txt"), "r", "right");
        let rung_name = format!("rung{i}");
        repo.jj(["new", "@", "@-"])?
            .create_change(&format!("m{i}.txt"), "m", "merge")
            .create_bookmark(&rung_name);
    }

    repo.jj(["new", &format!("rung{}", rungs - 1)])?
        .create_change("leaf.txt", "leaf", "leaf")
        .create_bookmark("leaf");

    // Drop the interior rung bookmarks so only `base` and `leaf` remain.
    for i in 0..rungs {
        repo.jj(["bookmark", "delete", &format!("rung{i}")])?;
    }

    let changes = repo.jj.log("base | leaf")?;
    let bookmarks: Vec<_> = BookmarkOrPending::from_changes(&changes)
        .into_iter()
        .collect();

    let before = repo.jj.exec_count();
    let graph = BookmarkGraph::from_bookmarks(&repo.jj, bookmarks.iter().cloned(), false)?;
    let spawned = repo.jj.exec_count() - before;

    // Linear bound: without the visited set this is exponential in `rungs`
    // (~514 invocations at 8 rungs) and the test would hang at a deeper ladder.
    // A generous linear ceiling still separates the two regimes cleanly.
    assert!(
        spawned <= 200,
        "resolving `leaf` over {rungs} merge rungs spawned {spawned} jj \
         invocations; expected a linear count (<=200). A combinatorial blow-up \
         means the visited-set dedup regressed."
    );

    // The fix preserves behavior: leaf resolves and its downstack reaches base
    // through the ladder.
    assert_some!(graph.find_bookmark_in_components("leaf"));
    let downstack = graph.downstack_of("leaf")?;
    assert_any!(downstack.iter(), |b: &BookmarkOrPending| b.name() == "base");

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
