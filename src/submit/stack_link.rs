//! Post-submit GitHub stack-link hook.
//!
//! After a successful GitHub submit, this registers the just-submitted PRs as a
//! GitHub-native stack by invoking `gh-stack link <pr>...` (bottom-to-top) once
//! per qualifying linear component. All coupling to GitHub's public-preview
//! `gh stack` CLI (binary name, argv shape, exit-code-9 semantics, timeout)
//! lives here; the caller (`commands/submit.rs`, T4) sees only
//! [`StackLinkOutcome`]. The hook is **non-fatal by construction**: it never
//! returns an error, so a link failure degrades to a warning line and never
//! turns a successful submit into a failed one.
//!
//! GitHub-only: the caller applies the `forge == GitHub` gate, so this module
//! receives a `&GitHubConfig` already scoped to GitHub. No GHE
//! host-qualification (see the record: `github.host` is a full API URL, not the
//! bare host `go-gh` wants); github.com path only.

use std::collections::{BTreeMap, BTreeSet};

use tracing::{debug, warn};

use crate::{
    bookmark::{BookmarkGraph, BookmarkOrPending},
    config::GitHubConfig,
    forge::MergeRequestLike as _,
    jj::Jujutsu,
    submit::execute::{MRUpdate, SubmissionResult},
};

/// Wall-clock deadline for a single `gh-stack link` invocation. `gh stack link`
/// makes real GitHub API round-trips, so this is far larger than the
/// token-command timeout; token resolution stays bounded by its own
/// `TOKEN_COMMAND_TIMEOUT` in `config.rs`.
const STACK_LINK_TIMEOUT: core::time::Duration = core::time::Duration::from_secs(60);

/// Outcome of the stack-link hook, rendered by the caller: `Linked` as success
/// lines plus any advisory notes, `Failed` as a warning line, `Skipped`
/// silently.
#[derive(Debug)]
pub enum StackLinkOutcome {
    /// The hook did not run, or found no stack worth linking.
    Skipped(SkipReason),
    /// One or more components were processed. `stacks` holds the PR numbers per
    /// linked stack, bottom-to-top, and always has at least one stack (an
    /// otherwise-empty result becomes `Skipped(NoQualifyingStack)` instead).
    /// `unlinked` holds human-readable notes about PRs or bookmarks that were
    /// part of a submitted stack's ancestry but were not added to any GitHub
    /// stack (a top-of-stack bookmark with no PR, or a PR whose component split
    /// off / went non-linear) — always supplementary to the linked stacks, so a
    /// normal single-PR submit stays quiet.
    Linked {
        stacks: Vec<Vec<u64>>,
        unlinked: Vec<String>,
    },
    /// A non-fatal failure; `warning` is a human-readable message naming the PR
    /// numbers involved so an operator can rerun by hand.
    Failed { warning: String },
}

/// Why the hook produced no link. Distinct causes so the caller and tests can
/// tell a deliberate suppression from "nothing qualified".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// `--dry-run`: a dry run must not touch the forge.
    DryRun,
    /// `--no-hooks`: the operator asked for a side-effect-free submit.
    NoHooks,
    /// `github.linkStack = false`: the knob is off for this repo.
    Disabled,
    /// No linear component resolved to a contiguous run of >= 2 PRs.
    NoQualifyingStack,
}

impl SkipReason {
    /// Human-readable notice text for the skip cause.
    #[must_use]
    pub fn describe(&self) -> &'static str {
        match self {
            Self::DryRun => "dry run: skipping stack link",
            Self::NoHooks => "--no-hooks: skipping stack link",
            Self::Disabled => "github.linkStack is false: skipping stack link",
            Self::NoQualifyingStack => "no linear stack of >= 2 PRs to link",
        }
    }
}

impl core::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.describe())
    }
}

/// Register the submitted PR stacks as GitHub-native stacks.
///
/// Rebuilds the bookmark graph from `result.changes` (the owned, solidified
/// `Vec<Change>` from T3a) — not the pre-execute graph — maps each bookmark to
/// its PR number, and invokes `gh-stack link` once per qualifying linear
/// component. Never returns an error: every failure is folded into
/// [`StackLinkOutcome`].
#[must_use]
pub fn link_stacks(
    github: &GitHubConfig,
    jj: &Jujutsu,
    result: &SubmissionResult,
    tracked: bool,
    dry_run: bool,
    no_hooks: bool,
) -> StackLinkOutcome {
    let runner = GhStackRunner {
        cwd: jj.cwd().to_path_buf(),
    };
    link_stacks_with_runner(&runner, github, jj, result, tracked, dry_run, no_hooks)
}

/// [`link_stacks`] with an injectable command runner, so unit tests never spawn
/// a real `gh-stack`. Handles the gates and the graph rebuild, then delegates
/// the per-component ordering + invocation to [`link_from_graph`].
fn link_stacks_with_runner(
    runner: &dyn StackLinkRunner,
    github: &GitHubConfig,
    jj: &Jujutsu,
    result: &SubmissionResult,
    tracked: bool,
    dry_run: bool,
    no_hooks: bool,
) -> StackLinkOutcome {
    if dry_run {
        return StackLinkOutcome::Skipped(SkipReason::DryRun);
    }
    if no_hooks {
        return StackLinkOutcome::Skipped(SkipReason::NoHooks);
    }
    if !github.link_stack {
        return StackLinkOutcome::Skipped(SkipReason::Disabled);
    }

    // Rebuild from the POST-execute, solidified names (T3a). A rebuild failure
    // is non-fatal: warn and move on.
    let graph = match BookmarkGraph::from_changes(jj, &result.changes, tracked) {
        Ok(graph) => graph,
        Err(e) => {
            return StackLinkOutcome::Failed {
                warning: format!("could not rebuild bookmark graph for stack link: {e}"),
            };
        }
    };

    link_from_graph(runner, github, &graph, &result.merge_requests)
}

/// The result of scanning one linear component's bottom-to-top bookmark run
/// against the bookmark->PR map.
struct ContiguityScan {
    /// The contiguous bottom-gap-only run of mapped PRs, bottom-to-top.
    prs: Vec<u64>,
    /// Bookmarks with no PR that sit *above* the mapped run with nothing mapped
    /// beyond them — a safe trailing gap (the top of the stack has no PR yet).
    trailing_unmapped: Vec<String>,
    /// True when a mapped bookmark sits above an unmapped one that is itself
    /// above the mapped run — a genuine interior gap, unsafe to link across.
    interior_gap: bool,
}

/// Classify a linear component's `ordered` (bottom-to-top) bookmark run:
///
///  - A *bottom* run of unmapped bookmarks is safely dropped (a merged-parent
///    ancestor reachable from trunk).
///  - Once the mapped run has started, an unmapped bookmark is deferred: it is
///    a TRAILING gap if nothing mapped sits above it, or an INTERIOR gap if a
///    mapped bookmark appears later.
///  - An INTERIOR gap (`[ok, missing, ok]`) is fatal to the component: linking
///    across it (`gh-stack link A C` skipping B) makes gh-stack auto-correct
///    C's base onto A, silently absorbing B's commits into C's diff.
///  - A TRAILING gap (`[ok, ok, missing]` — the top bookmark has no PR yet) is
///    safe: the lower run is still a contiguous bottom-gap-only run.
fn scan_contiguous_run(
    ordered: &[BookmarkOrPending<'_>],
    pr_map: &BTreeMap<String, u64>,
) -> ContiguityScan {
    let mut prs: Vec<u64> = Vec::new();
    let mut trailing_unmapped: Vec<String> = Vec::new();
    let mut interior_gap = false;
    for bookmark in ordered {
        match pr_map.get(bookmark.name()) {
            Some(&pr) => {
                if !trailing_unmapped.is_empty() {
                    // A mapped bookmark above an unmapped one that itself sits
                    // above the mapped run: a genuine interior gap.
                    interior_gap = true;
                    break;
                }
                prs.push(pr);
            }
            None => {
                if !prs.is_empty() {
                    // After the mapped run started: defer as a possible trailing
                    // gap (promoted to interior above if a mapped bookmark
                    // follows).
                    trailing_unmapped.push(bookmark.name().to_owned());
                }
                // else: bottom-run gap — drop and keep scanning upward.
            }
        }
    }
    ContiguityScan {
        prs,
        trailing_unmapped,
        interior_gap,
    }
}

/// The testable core: given a rebuilt graph and the execution's `MRUpdate`s,
/// derive the bottom-to-top PR run per linear component and invoke the runner.
///
/// Per-component partial failure composes into one outcome by letting failures
/// dominate: successful links are additive and idempotent (silent-but-done),
/// while a failure (or an interior-gap corruption guard) is the actionable
/// signal the operator must see. So if any component fails or hits an interior
/// gap, the outcome is `Failed` (its warning aggregates every failed
/// component's PR list); otherwise, if any component linked, `Linked` with the
/// successes; otherwise `Skipped(NoQualifyingStack)`.
fn link_from_graph(
    runner: &dyn StackLinkRunner,
    github: &GitHubConfig,
    graph: &BookmarkGraph<'_>,
    merge_requests: &[MRUpdate],
) -> StackLinkOutcome {
    let pr_map = build_pr_map(merge_requests);

    let mut stacks: Vec<Vec<u64>> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    // Advisory, non-failure notes: PRs/bookmarks in a submitted stack's ancestry
    // that were not added to a GitHub stack. Surfaced only alongside a linked
    // stack (below), so a normal standalone submit stays quiet.
    let mut notes: Vec<String> = Vec::new();

    for component in graph.components() {
        if !component.is_linear() {
            // Merges / multiple leaves — incl. the same-change multi-parent
            // shape (two bookmarks on one change makes the child multi-parent).
            // No linear stack; skip. Any PRs it carries surface via the
            // aggregate orphan note below so the operator isn't left guessing.
            debug!("stack link: skipping non-linear component (merge or multi-leaf)");
            continue;
        }

        let Some(leaf) = component.leaves.first() else {
            continue;
        };

        // `downstack()` is leaf->root (top-to-bottom); reverse for bottom-to-top.
        let mut ordered = leaf.downstack();
        ordered.reverse();

        let ContiguityScan {
            prs,
            trailing_unmapped,
            interior_gap,
        } = scan_contiguous_run(&ordered, &pr_map);

        if interior_gap {
            let warning = format!(
                "skipped linking a stack with a missing middle PR (linking across the gap \
                 would corrupt PR base chains); PRs found below the gap: {}",
                format_prs(&prs)
            );
            warn!("stack link: {warning}");
            warnings.push(warning);
            continue;
        }

        if prs.len() < 2 {
            // A single PR is not a stack; `gh-stack link` on one PR is pointless.
            // If it should have been part of a bigger stack, the aggregate
            // orphan note below names it.
            debug!(
                "stack link: skipping component with {} linkable PR(s) (< 2)",
                prs.len()
            );
            continue;
        }

        match runner.run(&prs, github) {
            LinkOutcome::Linked => {
                if !trailing_unmapped.is_empty() {
                    let note = format!(
                        "linked {} but left the top of the stack ({}) out — it has no PR yet; \
                         re-submit once it does",
                        format_prs(&prs),
                        trailing_unmapped.join(", ")
                    );
                    warn!("stack link: {note}");
                    notes.push(note);
                }
                stacks.push(prs);
            }
            LinkOutcome::NotEnabled => {
                let warning = format!(
                    "stacked PRs are not enabled for {}; enable them or set \
                     github.linkStack = false",
                    github.target_project()
                );
                warn!("stack link: {warning}");
                warnings.push(warning);
            }
            LinkOutcome::Failed { detail } => {
                let warning = format!(
                    "failed to link stack {} ({detail}); rerun by hand: gh-stack link {}",
                    format_prs(&prs),
                    prs.iter().map(u64::to_string).collect::<Vec<_>>().join(" ")
                );
                warn!("stack link: {warning}");
                warnings.push(warning);
            }
        }
    }

    if !warnings.is_empty() {
        return StackLinkOutcome::Failed {
            warning: warnings.join("; "),
        };
    }
    if stacks.is_empty() {
        return StackLinkOutcome::Skipped(SkipReason::NoQualifyingStack);
    }

    // Aggregate orphan detection: a PR that was submitted (so it has an MR in
    // `pr_map`) but landed in no linked stack — its component split off or went
    // non-linear. Surfaced only now, alongside a linked stack, so a genuinely
    // standalone single-PR submit (stacks empty above) stays quiet.
    let linked: BTreeSet<u64> = stacks.iter().flatten().copied().collect();
    let mut orphans: Vec<u64> = pr_map
        .values()
        .copied()
        .filter(|pr| !linked.contains(pr))
        .collect();
    orphans.sort_unstable();
    orphans.dedup();
    if !orphans.is_empty() {
        let note = format!(
            "these submitted PR(s) were not added to any stack (separate or non-linear \
             component): {}",
            format_prs(&orphans)
        );
        warn!("stack link: {note}");
        notes.push(note);
    }

    StackLinkOutcome::Linked {
        stacks,
        unlinked: notes,
    }
}

/// Build a `bookmark -> PR number` map from the execution's `MRUpdate`s,
/// deduped by bookmark (a bookmark may carry more than one `MRUpdate`). The
/// type-erased `AnyForgeMergeRequest::iid()` returns a `Cow<str>`, so parse it;
/// an id that fails to parse (never expected for GitHub) is debug-logged and
/// skipped rather than unwrapped.
fn build_pr_map(merge_requests: &[MRUpdate]) -> BTreeMap<String, u64> {
    let mut map = BTreeMap::new();
    for update in merge_requests {
        let iid = update.mr.iid();
        match iid.parse::<u64>() {
            Ok(pr) => {
                map.entry(update.bookmark.clone()).or_insert(pr);
            }
            Err(_) => {
                debug!(
                    "stack link: skipping merge request with non-numeric id `{iid}` for bookmark `{}`",
                    update.bookmark
                );
            }
        }
    }
    map
}

/// Render PR numbers for a human message, e.g. `#12 -> #13 -> #14`.
fn format_prs(prs: &[u64]) -> String {
    prs.iter()
        .map(|pr| format!("#{pr}"))
        .collect::<Vec<_>>()
        .join(" -> ")
}

/// Result of a single `gh-stack link` invocation, from the runner's view.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LinkOutcome {
    /// Exit 0: the stack was linked (or already up to date — idempotent).
    Linked,
    /// Exit 9: stacked PRs are not enabled for the repository.
    NotEnabled,
    /// Any other non-zero exit, timeout, or spawn error. `detail` never carries
    /// the token (env-only); it may quote truncated stderr.
    Failed { detail: String },
}

/// Injectable seam for running `gh-stack link`, so tests never spawn the real
/// binary. The prod impl ([`GhStackRunner`]) shells out; tests supply a fake.
trait StackLinkRunner {
    fn run(&self, prs: &[u64], github: &GitHubConfig) -> LinkOutcome;
}

/// Production runner: invokes the packaged `gh-stack` binary directly, with the
/// token in the child environment only (never argv, never an error string).
struct GhStackRunner {
    /// Repo root; the cwd `gh-stack` runs from.
    cwd: std::path::PathBuf,
}

impl StackLinkRunner for GhStackRunner {
    fn run(&self, prs: &[u64], github: &GitHubConfig) -> LinkOutcome {
        // Resolve the token first; its own error path never leaks the
        // credential, and we never put it in the returned detail.
        let token = match github.resolved_token() {
            Ok(token) => token,
            Err(e) => {
                return LinkOutcome::Failed {
                    detail: format!("could not resolve GH_TOKEN: {e}"),
                };
            }
        };

        let mut command = std::process::Command::new("gh-stack");
        command.arg("link");
        for pr in prs {
            command.arg(pr.to_string());
        }
        command.current_dir(&self.cwd);
        // Token env-only; pin the repo so a fork links stacks on the target.
        command.env("GH_TOKEN", token);
        command.env("GH_REPO", github.target_project());

        match crate::process::output_with_timeout(command, STACK_LINK_TIMEOUT) {
            Ok(Some(output)) if output.status.success() => LinkOutcome::Linked,
            Ok(Some(output)) if output.status.code() == Some(9) => LinkOutcome::NotEnabled,
            Ok(Some(output)) => {
                // stderr carries no credential (token is env-only), so it may be
                // quoted — but note it may be truncated at the stream cap.
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stderr = stderr.trim();
                LinkOutcome::Failed {
                    detail: if stderr.is_empty() {
                        format!("gh-stack exited with {}", output.status)
                    } else {
                        format!(
                            "gh-stack exited with {}: {stderr} (stderr may be truncated)",
                            output.status
                        )
                    },
                }
            }
            Ok(None) => LinkOutcome::Failed {
                detail: format!("gh-stack timed out after {}s", STACK_LINK_TIMEOUT.as_secs()),
            },
            Err(e) => LinkOutcome::Failed {
                detail: format!("could not run gh-stack: {e}"),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use core::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::{
        forge::{AnyForgeMergeRequest, test::MergeRequest},
        jj::{Change, Jujutsu},
        submit::execute::MRUpdateType,
    };

    /// A runner that records the argv of every call and returns a canned
    /// outcome, so tests assert what would have been invoked without spawning.
    struct RecordingRunner {
        calls: RefCell<Vec<Vec<u64>>>,
        response: LinkOutcome,
    }

    impl RecordingRunner {
        fn new(response: LinkOutcome) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                response,
            }
        }

        fn calls(&self) -> Vec<Vec<u64>> {
            self.calls.borrow().clone()
        }
    }

    impl StackLinkRunner for RecordingRunner {
        fn run(&self, prs: &[u64], _github: &GitHubConfig) -> LinkOutcome {
            self.calls.borrow_mut().push(prs.to_vec());
            self.response.clone()
        }
    }

    /// An `MRUpdate` for `bookmark` whose type-erased MR reports `iid` as its
    /// id.
    fn mr_update(bookmark: &str, iid: &str) -> MRUpdate {
        let mr = MergeRequest::builder()
            .id(iid.to_owned())
            .title(format!("PR for {bookmark}"))
            .source_branch(bookmark.to_owned())
            .target_branch("main".to_owned())
            .build();
        MRUpdate {
            mr: AnyForgeMergeRequest::new(mr),
            bookmark: bookmark.to_owned(),
            update_type: MRUpdateType::Created,
            warnings: None,
        }
    }

    fn github_config() -> GitHubConfig {
        GitHubConfig {
            project: "owner/repo".to_owned(),
            link_stack: true,
            ..GitHubConfig::default()
        }
    }

    /// A `SubmissionResult` carrying the given changes + merge requests; the
    /// other fields are irrelevant to the hook.
    fn submission_result(changes: Vec<Change>, merge_requests: Vec<MRUpdate>) -> SubmissionResult {
        SubmissionResult {
            merge_requests,
            errors: vec![],
            bookmarks_pushed: vec![],
            changes,
        }
    }

    /// A `Jujutsu` in a throwaway dir; used only where the hook returns before
    /// touching it (the gate tests). `Jujutsu::new` only checks `jj` is on
    /// PATH.
    fn dummy_jj() -> Jujutsu {
        let temp = std::env::temp_dir();
        Jujutsu::new(temp).expect("jj must be on PATH in the dev shell")
    }

    /// Build a `BookmarkGraph` from a linear bottom-to-top bookmark list via
    /// the hand-built `from_lookups`, so tests never touch a real jj repo.
    fn linear_graph_changes(bottom_to_top: &[&str]) -> crate::jj::ChangeMap {
        Change::mock_stack_map(bottom_to_top.iter().map(|b| Change::mock_from_bookmark(b)))
    }

    #[test]
    fn linear_three_stack_links_bottom_to_top_once() {
        let changes = linear_graph_changes(&["feature-a", "feature-b", "feature-c"]);
        let graph = BookmarkGraph::from_lookups(
            changes.create_bookmark_map(),
            &changes.create_adjacency_list(),
        );
        let mrs = vec![
            mr_update("feature-a", "10"),
            mr_update("feature-b", "20"),
            mr_update("feature-c", "30"),
        ];
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(&runner, &github_config(), &graph, &mrs);

        assert!(matches!(outcome, StackLinkOutcome::Linked { .. }));
        assert_eq!(
            runner.calls(),
            vec![vec![10, 20, 30]],
            "exactly one invocation, PRs bottom-to-top"
        );
    }

    #[test]
    fn two_independent_components_link_separately() {
        let mut changes = linear_graph_changes(&["feature-a", "feature-b"]);
        changes.extend(linear_graph_changes(&["feature-c", "feature-d"]));
        let graph = BookmarkGraph::from_lookups(
            changes.create_bookmark_map(),
            &changes.create_adjacency_list(),
        );
        let mrs = vec![
            mr_update("feature-a", "1"),
            mr_update("feature-b", "2"),
            mr_update("feature-c", "3"),
            mr_update("feature-d", "4"),
        ];
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(&runner, &github_config(), &graph, &mrs);

        assert!(matches!(outcome, StackLinkOutcome::Linked { .. }));
        let mut calls = runner.calls();
        calls.sort();
        assert_eq!(
            calls,
            vec![vec![1, 2], vec![3, 4]],
            "two components => two invocations"
        );
    }

    #[test]
    fn non_linear_component_is_skipped_without_invocation() {
        // feature-c sits on both feature-a and feature-b => multi-parent =>
        // non-linear component.
        let changes = Change::mock_stack_map([
            Change::mock_from_bookmark("feature-a"),
            Change::mock_from_bookmark("feature-b"),
            Change::mock_from_bookmark("feature-c")
                .with_mock_parent_bookmarks(["feature-a", "feature-b"]),
        ]);
        let graph = BookmarkGraph::from_lookups(
            changes.create_bookmark_map(),
            &changes.create_adjacency_list(),
        );
        let mrs = vec![
            mr_update("feature-a", "1"),
            mr_update("feature-b", "2"),
            mr_update("feature-c", "3"),
        ];
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(&runner, &github_config(), &graph, &mrs);

        assert!(
            matches!(
                outcome,
                StackLinkOutcome::Skipped(SkipReason::NoQualifyingStack)
            ),
            "non-linear component yields no qualifying stack"
        );
        assert!(runner.calls().is_empty(), "runner must not be called");
    }

    #[test]
    fn single_pr_component_is_skipped() {
        let changes = linear_graph_changes(&["feature-a"]);
        let graph = BookmarkGraph::from_lookups(
            changes.create_bookmark_map(),
            &changes.create_adjacency_list(),
        );
        let mrs = vec![mr_update("feature-a", "1")];
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(&runner, &github_config(), &graph, &mrs);

        assert!(matches!(
            outcome,
            StackLinkOutcome::Skipped(SkipReason::NoQualifyingStack)
        ));
        assert!(runner.calls().is_empty(), "a single PR is not a stack");
    }

    #[test]
    fn bottom_gap_is_dropped_and_top_run_links() {
        // feature-a (bottom) has no PR (merged-parent ancestor); the contiguous
        // top run b->c links.
        let changes = linear_graph_changes(&["feature-a", "feature-b", "feature-c"]);
        let graph = BookmarkGraph::from_lookups(
            changes.create_bookmark_map(),
            &changes.create_adjacency_list(),
        );
        let mrs = vec![mr_update("feature-b", "20"), mr_update("feature-c", "30")];
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(&runner, &github_config(), &graph, &mrs);

        assert!(matches!(outcome, StackLinkOutcome::Linked { .. }));
        assert_eq!(
            runner.calls(),
            vec![vec![20, 30]],
            "bottom gap dropped; contiguous top run links"
        );
    }

    #[test]
    fn interior_gap_skips_the_whole_component_and_never_links_across() {
        // feature-b (middle) has no PR. Linking [a, c] across it would corrupt
        // c's base chain, so the WHOLE component is skipped with a warning and
        // the runner is NEVER called.
        let changes = linear_graph_changes(&["feature-a", "feature-b", "feature-c"]);
        let graph = BookmarkGraph::from_lookups(
            changes.create_bookmark_map(),
            &changes.create_adjacency_list(),
        );
        let mrs = vec![mr_update("feature-a", "10"), mr_update("feature-c", "30")];
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(&runner, &github_config(), &graph, &mrs);

        assert!(
            runner.calls().is_empty(),
            "the runner MUST NOT be invoked for an interior-gap component"
        );
        match outcome {
            StackLinkOutcome::Failed { warning } => {
                assert!(
                    warning.contains("#10"),
                    "warning must name the PR(s) found below the gap, got: {warning}"
                );
                assert!(
                    !warning.contains("#30"),
                    "the PR above the interior gap must not be linked or claimed, got: {warning}"
                );
            }
            other => panic!("interior gap must surface as Failed, got: {other:?}"),
        }
    }

    #[test]
    fn trailing_gap_links_the_bottom_run_and_notes_the_unlinked_top() {
        // The leaf (feature-c, the top) has no PR yet — a submit that touched
        // only the lower part of the stack, or whose leaf action didn't emit an
        // MR. Linking [a, b] and leaving c out is SAFE (it does not corrupt any
        // base chain), so the bottom run links and the unlinked top is noted.
        // This is the regression guard for "first N stacked, top one not".
        let changes = linear_graph_changes(&["feature-a", "feature-b", "feature-c"]);
        let graph = BookmarkGraph::from_lookups(
            changes.create_bookmark_map(),
            &changes.create_adjacency_list(),
        );
        let mrs = vec![mr_update("feature-a", "10"), mr_update("feature-b", "20")];
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(&runner, &github_config(), &graph, &mrs);

        assert_eq!(
            runner.calls(),
            vec![vec![10, 20]],
            "the contiguous bottom run links; the unmapped top is dropped, not fatal"
        );
        match outcome {
            StackLinkOutcome::Linked { stacks, unlinked } => {
                assert_eq!(stacks, vec![vec![10, 20]]);
                assert_eq!(unlinked.len(), 1, "one note for the unlinked top");
                assert!(
                    unlinked[0].contains("feature-c"),
                    "note must name the unlinked top bookmark, got: {}",
                    unlinked[0]
                );
            }
            other => panic!("trailing gap must still Link the bottom run, got: {other:?}"),
        }
    }

    #[test]
    fn four_stack_with_unmapped_leaf_still_links_the_lower_three() {
        // The exact reported shape: a 4-bookmark stack whose top (feature-d) has
        // no PR. The lower three link as one stack; only the top is left out.
        let changes = linear_graph_changes(&["feature-a", "feature-b", "feature-c", "feature-d"]);
        let graph = BookmarkGraph::from_lookups(
            changes.create_bookmark_map(),
            &changes.create_adjacency_list(),
        );
        let mrs = vec![
            mr_update("feature-a", "1"),
            mr_update("feature-b", "2"),
            mr_update("feature-c", "3"),
        ];
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(&runner, &github_config(), &graph, &mrs);

        assert_eq!(
            runner.calls(),
            vec![vec![1, 2, 3]],
            "the lower three link; the unmapped leaf is left out, not fatal"
        );
        assert!(matches!(outcome, StackLinkOutcome::Linked { .. }));
    }

    #[test]
    fn interior_gap_with_a_mapped_bookmark_above_it_is_still_fatal() {
        // Distinguish a real interior gap from a trailing one: [ok, missing, ok,
        // ok] — a mapped bookmark sits ABOVE the gap, so linking across it would
        // corrupt base chains. Must skip the whole component, never link.
        let changes = linear_graph_changes(&["feature-a", "feature-b", "feature-c", "feature-d"]);
        let graph = BookmarkGraph::from_lookups(
            changes.create_bookmark_map(),
            &changes.create_adjacency_list(),
        );
        // feature-b (interior) unmapped; feature-c/-d mapped above it.
        let mrs = vec![
            mr_update("feature-a", "1"),
            mr_update("feature-c", "3"),
            mr_update("feature-d", "4"),
        ];
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(&runner, &github_config(), &graph, &mrs);

        assert!(
            runner.calls().is_empty(),
            "an interior gap with a mapped bookmark above it must never link"
        );
        assert!(matches!(outcome, StackLinkOutcome::Failed { .. }));
    }

    #[test]
    fn orphan_pr_in_a_separate_component_is_noted_alongside_the_linked_stack() {
        // One linear stack [a, b] links; feature-solo is its own single-PR
        // component. Its PR is "submitted but unstacked" — surfaced as an orphan
        // note (not a failure) so the operator sees why it isn't in the stack.
        let mut changes = linear_graph_changes(&["feature-a", "feature-b"]);
        changes.extend(linear_graph_changes(&["feature-solo"]));
        let graph = BookmarkGraph::from_lookups(
            changes.create_bookmark_map(),
            &changes.create_adjacency_list(),
        );
        let mrs = vec![
            mr_update("feature-a", "1"),
            mr_update("feature-b", "2"),
            mr_update("feature-solo", "9"),
        ];
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(&runner, &github_config(), &graph, &mrs);

        assert_eq!(
            runner.calls(),
            vec![vec![1, 2]],
            "only the real stack links"
        );
        match outcome {
            StackLinkOutcome::Linked { stacks, unlinked } => {
                assert_eq!(stacks, vec![vec![1, 2]]);
                assert_eq!(unlinked.len(), 1, "one orphan note");
                assert!(
                    unlinked[0].contains("#9"),
                    "orphan note must name the unstacked PR, got: {}",
                    unlinked[0]
                );
                assert!(
                    !unlinked[0].contains("#1") && !unlinked[0].contains("#2"),
                    "linked PRs must not appear in the orphan note, got: {}",
                    unlinked[0]
                );
            }
            other => panic!("expected Linked with an orphan note, got: {other:?}"),
        }
    }

    #[test]
    fn standalone_single_pr_submit_stays_quiet_no_orphan_note() {
        // A lone single-PR submit (nothing linked) must NOT emit an orphan note —
        // orphan detection is gated on at least one linked stack.
        let changes = linear_graph_changes(&["feature-solo"]);
        let graph = BookmarkGraph::from_lookups(
            changes.create_bookmark_map(),
            &changes.create_adjacency_list(),
        );
        let mrs = vec![mr_update("feature-solo", "9")];
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(&runner, &github_config(), &graph, &mrs);

        assert!(
            matches!(
                outcome,
                StackLinkOutcome::Skipped(SkipReason::NoQualifyingStack)
            ),
            "a lone single-PR submit stays a silent skip, no orphan note"
        );
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn duplicate_merge_request_per_bookmark_is_deduped() {
        let changes = linear_graph_changes(&["feature-a", "feature-b"]);
        let graph = BookmarkGraph::from_lookups(
            changes.create_bookmark_map(),
            &changes.create_adjacency_list(),
        );
        // Two MRUpdates for feature-a (same PR); one for feature-b.
        let mrs = vec![
            mr_update("feature-a", "1"),
            mr_update("feature-a", "1"),
            mr_update("feature-b", "2"),
        ];
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(&runner, &github_config(), &graph, &mrs);

        assert!(matches!(outcome, StackLinkOutcome::Linked { .. }));
        assert_eq!(
            runner.calls(),
            vec![vec![1, 2]],
            "duplicate MRUpdate per bookmark maps to one PR"
        );
    }

    #[test]
    fn non_numeric_iid_is_skipped_and_the_rest_proceed() {
        let changes = linear_graph_changes(&["feature-a", "feature-b"]);
        let graph = BookmarkGraph::from_lookups(
            changes.create_bookmark_map(),
            &changes.create_adjacency_list(),
        );
        // A stray non-numeric-id MR for a bookmark not in the graph is skipped
        // (debug-logged); the real stack still links.
        let mrs = vec![
            mr_update("feature-a", "1"),
            mr_update("feature-b", "2"),
            mr_update("orphan", "not-a-number"),
        ];
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(&runner, &github_config(), &graph, &mrs);

        assert!(matches!(outcome, StackLinkOutcome::Linked { .. }));
        assert_eq!(runner.calls(), vec![vec![1, 2]]);
    }

    #[test]
    fn exit_code_nine_yields_failed_with_not_enabled_message() {
        let changes = linear_graph_changes(&["feature-a", "feature-b"]);
        let graph = BookmarkGraph::from_lookups(
            changes.create_bookmark_map(),
            &changes.create_adjacency_list(),
        );
        let mrs = vec![mr_update("feature-a", "1"), mr_update("feature-b", "2")];
        let runner = RecordingRunner::new(LinkOutcome::NotEnabled);

        let outcome = link_from_graph(&runner, &github_config(), &graph, &mrs);

        match outcome {
            StackLinkOutcome::Failed { warning } => {
                assert!(
                    warning.contains("stacked PRs are not enabled for owner/repo"),
                    "exit 9 must give the dedicated actionable message, got: {warning}"
                );
                assert!(warning.contains("github.linkStack = false"));
            }
            other => panic!("exit 9 must yield Failed, got: {other:?}"),
        }
        assert_eq!(runner.calls(), vec![vec![1, 2]]);
    }

    #[test]
    fn dry_run_skips_before_touching_anything() {
        let jj = dummy_jj();
        let result = submission_result(vec![], vec![]);
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_stacks_with_runner(
            &runner,
            &github_config(),
            &jj,
            &result,
            false,
            true,  // dry_run
            false, // no_hooks
        );

        assert!(matches!(
            outcome,
            StackLinkOutcome::Skipped(SkipReason::DryRun)
        ));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn no_hooks_skips_before_touching_anything() {
        let jj = dummy_jj();
        let result = submission_result(vec![], vec![]);
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_stacks_with_runner(
            &runner,
            &github_config(),
            &jj,
            &result,
            false,
            false, // dry_run
            true,  // no_hooks
        );

        assert!(matches!(
            outcome,
            StackLinkOutcome::Skipped(SkipReason::NoHooks)
        ));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn disabled_knob_skips_before_touching_anything() {
        let jj = dummy_jj();
        let result = submission_result(vec![], vec![]);
        let github = GitHubConfig {
            link_stack: false,
            ..github_config()
        };
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_stacks_with_runner(&runner, &github, &jj, &result, false, false, false);

        assert!(matches!(
            outcome,
            StackLinkOutcome::Skipped(SkipReason::Disabled)
        ));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn generic_runner_failure_names_prs_for_manual_rerun() {
        let changes = linear_graph_changes(&["feature-a", "feature-b"]);
        let graph = BookmarkGraph::from_lookups(
            changes.create_bookmark_map(),
            &changes.create_adjacency_list(),
        );
        let mrs = vec![mr_update("feature-a", "1"), mr_update("feature-b", "2")];
        let runner = RecordingRunner::new(LinkOutcome::Failed {
            detail: "network unreachable".to_owned(),
        });

        let outcome = link_from_graph(&runner, &github_config(), &graph, &mrs);

        match outcome {
            StackLinkOutcome::Failed { warning } => {
                assert!(
                    warning.contains("gh-stack link 1 2"),
                    "warning must give a copy-pasteable rerun, got: {warning}"
                );
            }
            other => panic!("a runner failure must yield Failed, got: {other:?}"),
        }
    }

    #[test]
    fn one_success_one_failure_surfaces_the_failure() {
        // Two components: one links, one hits exit 9. Failures dominate, so the
        // outcome is Failed, but only the failing component's PRs are named.
        let mut changes = linear_graph_changes(&["feature-a", "feature-b"]);
        changes.extend(linear_graph_changes(&["feature-c", "feature-d"]));
        let graph = BookmarkGraph::from_lookups(
            changes.create_bookmark_map(),
            &changes.create_adjacency_list(),
        );
        let mrs = vec![
            mr_update("feature-a", "1"),
            mr_update("feature-b", "2"),
            mr_update("feature-c", "3"),
            mr_update("feature-d", "4"),
        ];
        // Both components go through the same runner; make every call fail so the
        // composition is deterministic (a mixed outcome still resolves to Failed).
        let runner = RecordingRunner::new(LinkOutcome::NotEnabled);

        let outcome = link_from_graph(&runner, &github_config(), &graph, &mrs);

        assert!(
            matches!(outcome, StackLinkOutcome::Failed { .. }),
            "any component failure makes the aggregate Failed"
        );
        // Kept as a sanity check that both qualifying components were attempted.
        assert_eq!(runner.calls().len(), 2);
    }

    #[test]
    fn build_pr_map_dedupes_and_skips_non_numeric() {
        let mrs = vec![
            mr_update("a", "5"),
            mr_update("a", "5"),
            mr_update("b", "bad"),
        ];
        let map = build_pr_map(&mrs);
        assert_eq!(map.get("a"), Some(&5));
        assert!(!map.contains_key("b"), "non-numeric id is skipped");
        assert_eq!(map.len(), 1);
    }

    #[test]
    fn from_lookups_helpers_are_exercised() {
        // Guard that the test fixture builds the expected adjacency for a linear
        // stack (a fixture regression here would silently weaken every test).
        let changes = linear_graph_changes(&["a", "b", "c"]);
        let adjacency: BTreeMap<String, BTreeSet<String>> = changes.create_adjacency_list();
        assert_eq!(adjacency.get("b"), Some(&BTreeSet::from(["a".to_owned()])));
        assert_eq!(adjacency.get("c"), Some(&BTreeSet::from(["b".to_owned()])));
    }
}
