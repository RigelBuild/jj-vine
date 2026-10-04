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
#[test]
fn plan_warns_for_unbookmarked_descendants() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    repo.create_change("bookmark.txt", "bookmark", "Bookmark B")
        .create_bookmark("B")
        .jj(["new"])?
        .create_change("child.txt", "child", "Unbookmarked child");
    let child = repo.jj.log("@")?.into_iter().next().unwrap();

    let warning = plan_and_capture_warnings(&repo)?;

    assert_contains!(warning, child.change_id_short());
    assert_contains!(warning, child.description_first_line());
    assert_contains!(
        warning,
        "will not be pushed unless a bookmark is moved onto it"
    );
    Ok(())
}

#[test]
fn plan_does_not_warn_for_bookmarked_descendants() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    repo.create_change("bookmark.txt", "bookmark", "Bookmark B")
        .create_bookmark("B")
        .jj(["new"])?
        .create_change("child.txt", "child", "Bookmarked child")
        .create_bookmark("child");

    let warning = plan_and_capture_warnings(&repo)?;

    assert_not_contains!(warning, "will not be pushed");
    Ok(())
}

#[test]
fn plan_warns_only_above_later_bookmarks() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    repo.create_change("b.txt", "b", "Bookmark B")
        .create_bookmark("B")
        .jj(["new"])?
        .create_change("x.txt", "x", "Unbookmarked X")
        .jj(["new"])?
        .create_change("c.txt", "c", "Bookmark C")
        .create_bookmark("C")
        .jj(["new"])?
        .create_change("y.txt", "y", "Unbookmarked Y");

    let middle_changes = repo.jj.log("B..C")?;
    let x = middle_changes
        .iter()
        .find(|change| change.description_first_line() == "Unbookmarked X")
        .expect("X is between the B and C bookmarks");
    let y = repo
        .jj
        .log("C::")?
        .into_iter()
        .find(|change| change.description_first_line() == "Unbookmarked Y")
        .expect("Y is above the C bookmark");
    let warning = plan_and_capture_warnings(&repo)?;

    assert_not_contains!(warning, x.change_id_short());
    assert_contains!(warning, y.change_id_short());
    Ok(())
}

#[test]
fn plan_warns_for_unbookmarked_change_without_description() -> Result<()> {
    let repo = TestRepo::with_local_remote();
    repo.create_change("bookmark.txt", "bookmark", "Bookmark B")
        .create_bookmark("B")
        .jj(["new"])?
        .create_change("child.txt", "child", "Unbookmarked child")
        .jj(["describe", "-m", ""])?;

    let warning = plan_and_capture_warnings(&repo)?;
    assert_contains!(warning, "(no description set)");
    Ok(())
}

fn plan_and_capture_warnings(repo: &TestRepo<TestRepo<()>>) -> Result<String> {
    use crate::{
        config::{Config, ForgeType},
        forge::{ForgeImpl, test::TestForge},
        output::BufferedOutput,
        submit::{PlanContext, plan::plan},
    };

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime");

    let changes = repo.jj.log("B")?;
    let graph = BookmarkGraph::from_changes(&repo.jj, changes.iter(), false)?;
    let forge = ForgeImpl::Test(TestForge::default());
    let config = Config::builder().forge(ForgeType::Forgejo).build();
    let output = BufferedOutput::new();

    runtime.block_on(plan(PlanContext {
        jj: &repo.jj,
        forge: &forge,
        config: &config,
        output: &output,
        bookmark_graph: &graph,
        dry_run: true,
    }))?;

    Ok(output.get_buffer())
}

/// Regression: a chain of merges must not re-walk shared ancestors once per
/// path. Each repeated expansion starts another `jj log` subprocess.
#[test]
fn merge_ladder_does_not_rewalk_combinatorially() -> Result<()> {
    let repo = TestRepo::new();

    // Each rung merges a child with its parent. That parent is reached twice:
    // directly from the merge and through the child. Temporary rung bookmarks
    // only navigate the setup; remove them before resolving leaf to base.
    repo.create_change("base.txt", "base", "base")
        .create_bookmark("base");

    // Eight rungs exceed 500 invocations without deduplication, but stay below
    // 200 with the visited set.
    let rungs: u32 = 8;
    for i in 0..rungs {
        let parent = if i == 0 {
            "base".to_owned()
        } else {
            format!("rung{}", i - 1)
        };
        repo.jj(["new", &parent])?
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

    // The pre-fix walk spawns over 500 subprocesses at eight rungs.
    // A generous linear ceiling separates the two regimes.
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

#[test]
fn merge_frontier_preserves_both_bookmarked_parents() -> Result<()> {
    let repo = TestRepo::new();
    repo.create_change("base.txt", "base", "base")
        .create_bookmark("base");
    repo.jj(["new", "base"])?
        .create_change("left.txt", "left", "left")
        .create_bookmark("left");
    repo.jj(["new", "base"])?
        .create_change("right.txt", "right", "right")
        .create_bookmark("right");
    repo.jj(["new", "left", "right"])?
        .create_change("merge.txt", "merge", "merge")
        .jj(["new", "@"])?
        .create_change("leaf.txt", "leaf", "leaf")
        .create_bookmark("leaf");
    repo.jj(["new", "@-"])?
        .create_change("peer.txt", "peer", "peer")
        .create_bookmark("peer");

    let changes = repo.jj.log("base | left | right | leaf | peer")?;
    let graph = BookmarkGraph::from_changes(&repo.jj, &changes, false)?;
    for child in ["leaf", "peer"] {
        let downstack = graph.downstack_of(child)?;
        assert_any!(downstack.iter(), |b: &BookmarkOrPending| b.name() == "left");
        assert_any!(downstack.iter(), |b: &BookmarkOrPending| b.name()
            == "right");
    }
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
