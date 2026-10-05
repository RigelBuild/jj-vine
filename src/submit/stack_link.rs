//! Register submitted GitHub pull requests as native stacks.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::OsString,
    path::{Path, PathBuf},
};

use tracing::{debug, warn};

use crate::{
    bookmark::{BookmarkGraph, BookmarkOrPending},
    config::GitHubConfig,
    forge::{AnyForgeMergeRequest, MergeRequestLike as _},
    jj::Jujutsu,
    submit::execute::{MRUpdate, SubmissionResult},
};

const STACK_LINK_TIMEOUT: core::time::Duration = core::time::Duration::from_secs(60);

/// Result of trying to register submitted pull requests as GitHub stacks.
#[derive(Debug)]
pub enum StackLinkOutcome {
    /// No stack link was attempted.
    Skipped(SkipReason),
    /// One or more stacks were linked, in bottom-to-top order. `warnings`
    /// lists components that could not be linked in the same run.
    Linked {
        stacks: Vec<Vec<u64>>,
        unlinked: Vec<String>,
        warnings: Vec<String>,
    },
    /// Stack linking failed. Submit itself remains successful.
    Failed { warning: String },
}

/// Why stack linking was skipped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// A dry run must not contact GitHub.
    DryRun,
    /// The user disabled post-submit hooks.
    NoHooks,
    /// `github.linkStack` is disabled.
    Disabled,
    /// No linear component has two or more pull requests to link.
    NoQualifyingStack,
    /// The optional `gh-stack` executable is not installed.
    MissingBinary,
}

impl SkipReason {
    #[must_use]
    pub fn describe(&self) -> &'static str {
        match self {
            Self::DryRun => "dry run: skipping stack link",
            Self::NoHooks => "--no-hooks: skipping stack link",
            Self::Disabled => "github.linkStack is false: skipping stack link",
            Self::NoQualifyingStack => "no linear stack of two or more PRs to link",
            Self::MissingBinary => "gh-stack is not installed: skipping stack link",
        }
    }
}

impl core::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.describe())
    }
}

/// Link the submitted pull requests into GitHub-native stacks.
///
/// `existing_merge_requests` are the pull requests found at planning time
/// (`SubmissionPlan::existing_mrs`), so a PR that execution left unchanged
/// still takes its place in the stack.
///
/// This hook never returns an error. Failures are represented as outcomes so
/// they cannot turn a successful submit into a failed command.
#[must_use]
pub fn link_stacks(
    github: &GitHubConfig,
    jj: &Jujutsu,
    result: &SubmissionResult,
    existing_merge_requests: &HashMap<String, AnyForgeMergeRequest>,
    tracked: bool,
    dry_run: bool,
    no_hooks: bool,
) -> StackLinkOutcome {
    let runner = GhStackRunner {
        cwd: jj.cwd().to_path_buf(),
        search_path: std::env::var_os("PATH"),
    };
    link_stacks_with_runner(
        &runner,
        github,
        jj,
        result,
        existing_merge_requests,
        tracked,
        dry_run,
        no_hooks,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "mirrors link_stacks plus the runner seam"
)]
fn link_stacks_with_runner(
    runner: &dyn StackLinkRunner,
    github: &GitHubConfig,
    jj: &Jujutsu,
    result: &SubmissionResult,
    existing_merge_requests: &HashMap<String, AnyForgeMergeRequest>,
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

    let graph = match BookmarkGraph::from_changes(jj, &result.changes, tracked) {
        Ok(graph) => graph,
        Err(error) => {
            return StackLinkOutcome::Failed {
                warning: format!("could not rebuild bookmark graph for stack link: {error}"),
            };
        }
    };

    let pr_map = build_pr_map(existing_merge_requests, &result.merge_requests);
    link_from_graph(runner, github, &graph, &pr_map)
}

struct ContiguityScan {
    prs: Vec<u64>,
    trailing_unmapped: Vec<String>,
    interior_gap: bool,
}

fn scan_contiguous_run(
    ordered: &[BookmarkOrPending<'_>],
    pr_map: &BTreeMap<String, u64>,
) -> ContiguityScan {
    let mut prs = Vec::new();
    let mut trailing_unmapped = Vec::new();
    let mut interior_gap = false;

    for bookmark in ordered {
        match pr_map.get(bookmark.name()) {
            Some(&pr) if trailing_unmapped.is_empty() => prs.push(pr),
            Some(_) => {
                interior_gap = true;
                break;
            }
            None if !prs.is_empty() => trailing_unmapped.push(bookmark.name().to_owned()),
            None => {}
        }
    }

    ContiguityScan {
        prs,
        trailing_unmapped,
        interior_gap,
    }
}

fn link_from_graph(
    runner: &dyn StackLinkRunner,
    github: &GitHubConfig,
    graph: &BookmarkGraph<'_>,
    pr_map: &BTreeMap<String, u64>,
) -> StackLinkOutcome {
    let mut stacks = Vec::new();
    let mut warnings = Vec::new();
    let mut notes = Vec::new();
    // PRs already named in a warning, so they are not listed again as orphans.
    let mut reported = BTreeSet::new();

    for component in graph.components() {
        if !component.is_linear() {
            debug!("stack link: skipping a non-linear component");
            continue;
        }
        let Some(leaf) = component.leaves.first() else {
            continue;
        };
        let mut ordered = leaf.downstack();
        ordered.reverse();
        let ContiguityScan {
            prs,
            trailing_unmapped,
            interior_gap,
        } = scan_contiguous_run(&ordered, pr_map);

        if interior_gap {
            let warning = format!(
                "skipped linking a stack with a missing middle PR; PRs below the gap: {}",
                format_prs(&prs)
            );
            warn!("stack link: {warning}");
            warnings.push(warning);
            reported.extend(prs.iter().copied());
            continue;
        }
        if prs.len() < 2 {
            debug!("stack link: no qualifying PR stack in component");
            continue;
        }

        match runner.run(&prs, github) {
            LinkOutcome::Linked => {
                if !trailing_unmapped.is_empty() {
                    let note = format!(
                        "linked {} but left the top of the stack ({}) out because it has no PR yet",
                        format_prs(&prs),
                        trailing_unmapped.join(", ")
                    );
                    warn!("stack link: {note}");
                    notes.push(note);
                }
                stacks.push(prs);
            }
            LinkOutcome::NotEnabled => {
                warnings.push(format!(
                    "stacked PRs are not enabled for {}; enable them or set github.linkStack = false",
                    github.target_project()
                ));
                reported.extend(prs.iter().copied());
            }
            LinkOutcome::MissingBinary => {
                if stacks.is_empty() && warnings.is_empty() {
                    return StackLinkOutcome::Skipped(SkipReason::MissingBinary);
                }
                warnings.push(
                    "gh-stack is no longer on PATH; remaining stacks were not linked".to_owned(),
                );
                break;
            }
            LinkOutcome::Failed { detail } => {
                warnings.push(format!(
                    "failed to link stack {} ({detail}); rerun by hand: gh-stack link {}",
                    format_prs(&prs),
                    prs.iter().map(u64::to_string).collect::<Vec<_>>().join(" ")
                ));
                reported.extend(prs.iter().copied());
            }
        }
    }

    if stacks.is_empty() {
        if warnings.is_empty() {
            return StackLinkOutcome::Skipped(SkipReason::NoQualifyingStack);
        }
        return StackLinkOutcome::Failed {
            warning: warnings.join("; "),
        };
    }

    let linked: BTreeSet<u64> = stacks.iter().flatten().copied().collect();
    let mut orphans: Vec<u64> = pr_map
        .values()
        .copied()
        .filter(|pr| !linked.contains(pr) && !reported.contains(pr))
        .collect();
    orphans.sort_unstable();
    orphans.dedup();
    if !orphans.is_empty() {
        let note = format!(
            "these submitted PRs were not added to a stack: {}",
            format_prs(&orphans)
        );
        warn!("stack link: {note}");
        notes.push(note);
    }

    StackLinkOutcome::Linked {
        stacks,
        unlinked: notes,
        warnings,
    }
}

/// Map each bookmark to its PR number: PRs found at planning time, overlaid
/// with the PRs execution created or updated.
fn build_pr_map(
    existing_merge_requests: &HashMap<String, AnyForgeMergeRequest>,
    merge_requests: &[MRUpdate],
) -> BTreeMap<String, u64> {
    let existing = existing_merge_requests
        .iter()
        .map(|(bookmark, mr)| (bookmark, mr));
    let updated = merge_requests
        .iter()
        .map(|update| (&update.bookmark, &update.mr));

    let mut map = BTreeMap::new();
    for (bookmark, mr) in existing.chain(updated) {
        match mr.iid().parse::<u64>() {
            Ok(pr) => {
                map.insert(bookmark.clone(), pr);
            }
            Err(_) => {
                debug!("stack link: ignoring non-numeric pull request id for bookmark {bookmark}")
            }
        }
    }
    map
}

fn format_prs(prs: &[u64]) -> String {
    prs.iter()
        .map(|pr| format!("#{pr}"))
        .collect::<Vec<_>>()
        .join(" -> ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LinkOutcome {
    Linked,
    NotEnabled,
    MissingBinary,
    Failed { detail: String },
}

trait StackLinkRunner {
    fn run(&self, prs: &[u64], github: &GitHubConfig) -> LinkOutcome;
}

struct GhStackRunner {
    cwd: PathBuf,
    /// The `PATH` searched for `gh-stack`.
    search_path: Option<OsString>,
}

impl GhStackRunner {
    fn run_with_binary(&self, binary: &Path, prs: &[u64], github: &GitHubConfig) -> LinkOutcome {
        let token = match github.resolved_token() {
            Ok(token) => token,
            Err(error) => {
                return LinkOutcome::Failed {
                    detail: format!("could not resolve GH_TOKEN: {error}"),
                };
            }
        };

        let mut command = std::process::Command::new(binary);
        command.arg("link");
        for pr in prs {
            command.arg(pr.to_string());
        }
        command.current_dir(&self.cwd);
        command.env("GH_TOKEN", &token);
        command.env("GH_REPO", github.target_project());

        match crate::process::output_with_timeout(command, STACK_LINK_TIMEOUT) {
            Ok(Some(output)) if output.status.success() => LinkOutcome::Linked,
            Ok(Some(output)) if output.status.code() == Some(9) => LinkOutcome::NotEnabled,
            Ok(Some(output)) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stderr = stderr.replace(&token, "[redacted]");
                let stderr = stderr.trim();
                LinkOutcome::Failed {
                    detail: if stderr.is_empty() {
                        format!("gh-stack exited with {}", output.status)
                    } else {
                        format!("gh-stack exited with {}: {stderr}", output.status)
                    },
                }
            }
            Ok(None) => LinkOutcome::Failed {
                detail: format!("gh-stack timed out after {}s", STACK_LINK_TIMEOUT.as_secs()),
            },
            // The binary was found on PATH, so a spawn failure here (including
            // NotFound for a missing interpreter or a racing uninstall) is a
            // real failure the user should see, not a silent skip.
            Err(error) => LinkOutcome::Failed {
                detail: format!("could not run gh-stack at {}: {error}", binary.display()),
            },
        }
    }
}

impl StackLinkRunner for GhStackRunner {
    fn run(&self, prs: &[u64], github: &GitHubConfig) -> LinkOutcome {
        let Ok(binary) = which::which_in("gh-stack", self.search_path.as_ref(), &self.cwd) else {
            return LinkOutcome::MissingBinary;
        };
        self.run_with_binary(&binary, prs, github)
    }
}

#[cfg(test)]
mod tests {
    use core::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::{
        forge::{AnyForgeMergeRequest, test::MergeRequest},
        jj::Change,
        submit::execute::MRUpdateType,
    };

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

    /// Answers each call with the next queued outcome, in call order.
    struct SequencedRunner {
        calls: RefCell<Vec<Vec<u64>>>,
        responses: RefCell<std::collections::VecDeque<LinkOutcome>>,
    }

    impl SequencedRunner {
        fn new(responses: impl IntoIterator<Item = LinkOutcome>) -> Self {
            Self {
                calls: RefCell::new(Vec::new()),
                responses: RefCell::new(responses.into_iter().collect()),
            }
        }
    }

    impl StackLinkRunner for SequencedRunner {
        fn run(&self, prs: &[u64], _github: &GitHubConfig) -> LinkOutcome {
            self.calls.borrow_mut().push(prs.to_vec());
            self.responses
                .borrow_mut()
                .pop_front()
                .expect("a queued response for every runner call")
        }
    }

    fn pr_map(updates: &[MRUpdate]) -> BTreeMap<String, u64> {
        build_pr_map(&HashMap::new(), updates)
    }

    fn existing_mr(bookmark: &str, iid: &str) -> (String, AnyForgeMergeRequest) {
        (bookmark.to_owned(), mr_update(bookmark, iid).mr)
    }

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
            token: "test-token".to_owned(),
            link_stack: true,
            ..GitHubConfig::default()
        }
    }

    fn linear_changes(bottom_to_top: &[&str]) -> crate::jj::ChangeMap {
        Change::mock_stack_map(
            bottom_to_top
                .iter()
                .map(|bookmark| Change::mock_from_bookmark(bookmark)),
        )
    }

    fn graph(changes: &crate::jj::ChangeMap) -> BookmarkGraph<'_> {
        BookmarkGraph::from_lookups(
            changes.create_bookmark_map(),
            &changes.create_adjacency_list(),
        )
    }

    #[test]
    fn binary_absent_from_path_is_a_silent_skip() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let runner = GhStackRunner {
            cwd: temp.path().to_path_buf(),
            search_path: Some(temp.path().as_os_str().to_owned()),
        };

        let outcome = runner.run(&[1, 2], &github_config());

        assert_eq!(
            outcome,
            LinkOutcome::MissingBinary,
            "nothing named gh-stack on PATH"
        );
    }

    #[test]
    fn spawn_not_found_after_lookup_is_a_warning() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let runner = GhStackRunner {
            cwd: temp.path().to_path_buf(),
            search_path: None,
        };
        let vanished_binary = temp.path().join("gh-stack");

        let outcome = runner.run_with_binary(&vanished_binary, &[1, 2], &github_config());

        assert!(
            matches!(&outcome, LinkOutcome::Failed { detail } if detail.contains("could not run gh-stack")),
            "a binary found on PATH that fails to spawn must warn, got {outcome:?}"
        );
    }

    #[test]
    fn missing_binary_outcome_is_skipped_not_failed() {
        let changes = linear_changes(&["a", "b"]);
        let runner = RecordingRunner::new(LinkOutcome::MissingBinary);

        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[mr_update("a", "10"), mr_update("b", "20")]),
        );

        assert!(matches!(
            outcome,
            StackLinkOutcome::Skipped(SkipReason::MissingBinary)
        ));
    }

    #[test]
    fn stack_links_pull_requests_bottom_to_top() {
        let changes = linear_changes(&["a", "b", "c"]);
        let runner = RecordingRunner::new(LinkOutcome::Linked);
        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[
                mr_update("a", "10"),
                mr_update("b", "20"),
                mr_update("c", "30"),
            ]),
        );

        assert!(matches!(
            outcome,
            StackLinkOutcome::Linked { stacks, .. } if stacks == vec![vec![10, 20, 30]]
        ));
        assert_eq!(runner.calls(), vec![vec![10, 20, 30]]);
    }

    #[test]
    fn interior_missing_pull_request_never_links_across_gap() {
        let changes = linear_changes(&["a", "b", "c"]);
        let runner = RecordingRunner::new(LinkOutcome::Linked);
        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[mr_update("a", "10"), mr_update("c", "30")]),
        );

        assert!(runner.calls().is_empty());
        assert!(matches!(outcome, StackLinkOutcome::Failed { warning } if warning.contains("#10")));
    }

    #[test]
    fn trailing_missing_pull_request_keeps_lower_stack() {
        let changes = linear_changes(&["a", "b", "c"]);
        let runner = RecordingRunner::new(LinkOutcome::Linked);
        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[mr_update("a", "10"), mr_update("b", "20")]),
        );

        assert_eq!(runner.calls(), vec![vec![10, 20]]);
        assert!(matches!(
            outcome,
            StackLinkOutcome::Linked { unlinked, .. } if unlinked[0].contains("c")
        ));
    }

    #[test]
    fn single_pull_request_is_skipped() {
        let changes = linear_changes(&["a"]);
        let runner = RecordingRunner::new(LinkOutcome::Linked);
        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[mr_update("a", "1")]),
        );

        assert!(matches!(
            outcome,
            StackLinkOutcome::Skipped(SkipReason::NoQualifyingStack)
        ));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn dry_run_and_no_hooks_skip_before_graph_rebuild() {
        let jj = Jujutsu::new(std::env::temp_dir()).expect("jj on PATH");
        let result = SubmissionResult {
            merge_requests: vec![],
            errors: vec![],
            bookmarks_pushed: vec![],
            changes: vec![],
        };
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        assert!(matches!(
            link_stacks_with_runner(
                &runner,
                &github_config(),
                &jj,
                &result,
                &HashMap::new(),
                false,
                true,
                false
            ),
            StackLinkOutcome::Skipped(SkipReason::DryRun)
        ));
        assert!(matches!(
            link_stacks_with_runner(
                &runner,
                &github_config(),
                &jj,
                &result,
                &HashMap::new(),
                false,
                false,
                true
            ),
            StackLinkOutcome::Skipped(SkipReason::NoHooks)
        ));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn disabled_stack_link_is_skipped_before_graph_rebuild() {
        let jj = Jujutsu::new(std::env::temp_dir()).expect("jj on PATH");
        let result = SubmissionResult {
            merge_requests: vec![],
            errors: vec![],
            bookmarks_pushed: vec![],
            changes: vec![],
        };
        let runner = RecordingRunner::new(LinkOutcome::Linked);
        let mut github = github_config();
        github.link_stack = false;

        assert!(matches!(
            link_stacks_with_runner(
                &runner,
                &github,
                &jj,
                &result,
                &HashMap::new(),
                false,
                false,
                false
            ),
            StackLinkOutcome::Skipped(SkipReason::Disabled)
        ));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn merge_request_map_deduplicates_and_ignores_non_numeric_ids() {
        let map = pr_map(&[
            mr_update("a", "5"),
            mr_update("a", "5"),
            mr_update("b", "not-a-number"),
        ]);
        assert_eq!(map, BTreeMap::from([("a".to_owned(), 5)]));
    }

    #[test]
    fn merge_request_map_includes_unchanged_existing_pull_requests() {
        let existing = HashMap::from([existing_mr("a", "10"), existing_mr("b", "20")]);

        let map = build_pr_map(&existing, &[mr_update("c", "30")]);

        assert_eq!(
            map,
            BTreeMap::from([
                ("a".to_owned(), 10),
                ("b".to_owned(), 20),
                ("c".to_owned(), 30),
            ]),
            "planned PRs that execution left alone must still map"
        );
    }

    #[test]
    fn unchanged_existing_middle_pull_request_does_not_create_a_gap() {
        let changes = linear_changes(&["a", "b", "c"]);
        let runner = RecordingRunner::new(LinkOutcome::Linked);
        // With descriptions disabled, `b` already has a PR and gets no action.
        let existing = HashMap::from([existing_mr("a", "10"), existing_mr("b", "20")]);

        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &build_pr_map(&existing, &[mr_update("c", "30")]),
        );

        assert_eq!(runner.calls(), vec![vec![10, 20, 30]], "whole stack linked");
        assert!(
            matches!(&outcome, StackLinkOutcome::Linked { stacks, .. } if *stacks == vec![vec![10, 20, 30]]),
            "unexpected outcome {outcome:?}"
        );
    }

    #[test]
    fn adjacency_fixture_is_linear() {
        let changes = linear_changes(&["a", "b", "c"]);
        let adjacency: BTreeMap<String, BTreeSet<String>> = changes.create_adjacency_list();
        assert_eq!(adjacency.get("b"), Some(&BTreeSet::from(["a".to_owned()])));
        assert_eq!(adjacency.get("c"), Some(&BTreeSet::from(["b".to_owned()])));
    }

    #[test]
    fn independent_linear_components_link_separately() {
        let mut changes = linear_changes(&["a", "b"]);
        changes.extend(linear_changes(&["c", "d"]));
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[
                mr_update("a", "1"),
                mr_update("b", "2"),
                mr_update("c", "3"),
                mr_update("d", "4"),
            ]),
        );

        let mut calls = runner.calls();
        calls.sort();
        assert_eq!(calls, vec![vec![1, 2], vec![3, 4]]);
        assert!(matches!(outcome, StackLinkOutcome::Linked { .. }));
    }

    #[test]
    fn non_linear_component_is_skipped() {
        let changes = Change::mock_stack_map([
            Change::mock_from_bookmark("a"),
            Change::mock_from_bookmark("b"),
            Change::mock_from_bookmark("c").with_mock_parent_bookmarks(["a", "b"]),
        ]);
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[
                mr_update("a", "1"),
                mr_update("b", "2"),
                mr_update("c", "3"),
            ]),
        );

        assert!(matches!(
            outcome,
            StackLinkOutcome::Skipped(SkipReason::NoQualifyingStack)
        ));
        assert!(runner.calls().is_empty());
    }

    #[test]
    fn missing_bottom_pr_does_not_block_contiguous_top_run() {
        let changes = linear_changes(&["a", "b", "c"]);
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[mr_update("b", "20"), mr_update("c", "30")]),
        );

        assert_eq!(runner.calls(), vec![vec![20, 30]]);
        assert!(matches!(outcome, StackLinkOutcome::Linked { .. }));
    }

    #[test]
    fn a_second_missing_pr_above_an_interior_gap_still_blocks_linking() {
        let changes = linear_changes(&["a", "b", "c", "d"]);
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[
                mr_update("a", "1"),
                mr_update("c", "3"),
                mr_update("d", "4"),
            ]),
        );

        assert!(runner.calls().is_empty());
        assert!(matches!(outcome, StackLinkOutcome::Failed { .. }));
    }

    #[test]
    fn separate_single_pr_is_reported_when_another_component_links() {
        let mut changes = linear_changes(&["a", "b"]);
        changes.extend(linear_changes(&["solo"]));
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[
                mr_update("a", "1"),
                mr_update("b", "2"),
                mr_update("solo", "9"),
            ]),
        );

        assert!(matches!(
            outcome,
            StackLinkOutcome::Linked { stacks, unlinked, .. }
                if stacks == vec![vec![1, 2]]
                    && unlinked.len() == 1
                    && unlinked[0].contains("#9")
        ));
    }

    #[test]
    fn standalone_pr_is_not_reported_as_an_orphan() {
        let changes = linear_changes(&["solo"]);
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[mr_update("solo", "9")]),
        );

        assert!(matches!(
            outcome,
            StackLinkOutcome::Skipped(SkipReason::NoQualifyingStack)
        ));
    }

    #[test]
    fn not_enabled_returns_actionable_warning() {
        let changes = linear_changes(&["a", "b"]);
        let runner = RecordingRunner::new(LinkOutcome::NotEnabled);

        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[mr_update("a", "1"), mr_update("b", "2")]),
        );

        assert!(matches!(
            outcome,
            StackLinkOutcome::Failed { warning }
                if warning.contains("stacked PRs are not enabled for owner/repo")
                    && warning.contains("github.linkStack = false")
        ));
    }

    #[test]
    fn runner_failure_includes_manual_rerun_command() {
        let changes = linear_changes(&["a", "b"]);
        let runner = RecordingRunner::new(LinkOutcome::Failed {
            detail: "network unreachable".to_owned(),
        });

        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[mr_update("a", "1"), mr_update("b", "2")]),
        );

        assert!(matches!(
            outcome,
            StackLinkOutcome::Failed { warning }
                if warning.contains("gh-stack link 1 2")
                    && warning.contains("network unreachable")
        ));
    }

    #[test]
    fn failure_in_one_component_does_not_hide_earlier_successes_or_later_attempts() {
        let mut changes = linear_changes(&["a", "b"]);
        changes.extend(linear_changes(&["c", "d"]));
        let runner = RecordingRunner::new(LinkOutcome::NotEnabled);

        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[
                mr_update("a", "1"),
                mr_update("b", "2"),
                mr_update("c", "3"),
                mr_update("d", "4"),
            ]),
        );

        assert_eq!(runner.calls().len(), 2, "both components attempted");
        assert!(matches!(
            outcome,
            StackLinkOutcome::Failed { warning }
                if warning.matches("not enabled").count() == 2
        ));
    }

    #[test]
    fn mixed_outcomes_keep_linked_stacks_notes_and_warnings() {
        let mut changes = linear_changes(&["a", "b", "top"]);
        changes.extend(linear_changes(&["c", "d"]));
        changes.extend(linear_changes(&["solo"]));
        let runner = SequencedRunner::new([
            LinkOutcome::Linked,
            LinkOutcome::Failed {
                detail: "network unreachable".to_owned(),
            },
        ]);

        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[
                mr_update("a", "1"),
                mr_update("b", "2"),
                mr_update("c", "3"),
                mr_update("d", "4"),
                mr_update("solo", "9"),
            ]),
        );

        let calls = runner.calls.borrow().clone();
        assert_eq!(calls.len(), 2, "both qualifying components attempted");
        let (linked, failed) = (&calls[0], &calls[1]);
        let failed_args = failed
            .iter()
            .map(u64::to_string)
            .collect::<Vec<_>>()
            .join(" ");
        let StackLinkOutcome::Linked {
            stacks,
            unlinked,
            warnings,
        } = outcome
        else {
            panic!("a successful component must survive a later failure, got {outcome:?}");
        };
        assert_eq!(stacks, vec![linked.clone()], "successful stack ids kept");
        assert_eq!(warnings.len(), 1, "one warning for the failed component");
        assert!(
            warnings[0].contains("network unreachable")
                && warnings[0].contains(&format!("gh-stack link {failed_args}")),
            "failure warning keeps rerun command: {warnings:?}"
        );
        assert!(
            unlinked.iter().any(|note| note.contains("#9")),
            "standalone PR is still reported: {unlinked:?}"
        );
        assert!(
            !unlinked
                .iter()
                .any(|note| failed.iter().any(|pr| note.contains(&format!("#{pr}")))),
            "PRs named in a warning are not repeated as orphans: {unlinked:?}"
        );
        if *linked == vec![1, 2] {
            assert!(
                unlinked.iter().any(|note| note.contains("top")),
                "trailing-bookmark note kept: {unlinked:?}"
            );
        }
    }

    #[test]
    fn binary_vanishing_mid_run_keeps_earlier_stacks() {
        let mut changes = linear_changes(&["a", "b"]);
        changes.extend(linear_changes(&["c", "d"]));
        let runner = SequencedRunner::new([LinkOutcome::Linked, LinkOutcome::MissingBinary]);

        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[
                mr_update("a", "1"),
                mr_update("b", "2"),
                mr_update("c", "3"),
                mr_update("d", "4"),
            ]),
        );

        assert!(
            matches!(&outcome, StackLinkOutcome::Linked { stacks, warnings, .. }
                if stacks.len() == 1 && warnings.len() == 1 && warnings[0].contains("no longer on PATH")),
            "unexpected outcome {outcome:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stderr_does_not_expose_github_token() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().expect("temporary directory");
        let binary = temp.path().join("gh-stack-test");
        std::fs::write(
            &binary,
            "#!/bin/sh\nprintf '%s\\n' \"$GH_TOKEN\" >&2\nexit 1\n",
        )
        .expect("write helper");
        let mut permissions = std::fs::metadata(&binary)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&binary, permissions).expect("make helper executable");

        let outcome = GhStackRunner {
            cwd: temp.path().to_path_buf(),
            search_path: None,
        }
        .run_with_binary(&binary, &[1, 2], &github_config());

        assert!(matches!(
            outcome,
            LinkOutcome::Failed { detail }
                if detail.contains("[redacted]") && !detail.contains("test-token")
        ));
    }

    /// Drive the real plan → execute → stack-link handoff with descriptions
    /// and title sync disabled, so unchanged existing PRs get no MR action and
    /// the top of the stack is a new bookmark created by the push.
    #[tokio::test]
    async fn production_handoff_links_unchanged_existing_and_created_pull_requests() {
        use std::collections::HashSet;

        use crate::{
            config::{Config, DescriptionConfig, ForgeType, TitleConfig},
            forge::{ForgeImpl, test::TestForge},
            output::BufferedOutput,
            submit::{
                PlanContext,
                RootExecuteContext,
                execute::execute,
                find_changes_to_submit,
                plan::plan,
            },
            tests::TestRepo,
        };

        let repo = TestRepo::with_local_remote();
        repo.create_change("a.txt", "a", "A").create_bookmark("a");
        repo.push_bookmark("a");
        repo.exec(["new"]);
        repo.create_change("b.txt", "b", "B").create_bookmark("b");
        repo.push_bookmark("b");
        repo.exec(["new"]);
        repo.create_change("c.txt", "c", "C");
        let pending_change = repo
            .jj
            .log("@")
            .expect("read top change")
            .into_iter()
            .next()
            .expect("top change exists")
            .change_id;

        let mut forge = TestForge::builder().build();
        for (id, source, target) in [("10", "a", "main"), ("20", "b", "a")] {
            forge.add_merge_request(
                MergeRequest::builder()
                    .id(id.to_owned())
                    .title(format!("PR {source}"))
                    .source_branch(source.to_owned())
                    .target_branch(target.to_owned())
                    .build(),
            );
        }
        let forge = ForgeImpl::Test(forge);
        let config = Config::builder()
            .forge(ForgeType::GitHub)
            .description(DescriptionConfig {
                enabled: false,
                ..DescriptionConfig::default()
            })
            .title(TitleConfig {
                sync_single_revision: false,
                sync_multiple_revisions: false,
                ..TitleConfig::default()
            })
            .github(github_config())
            .build();
        let output = BufferedOutput::new();

        let pending = HashSet::from([pending_change]);
        let changes =
            find_changes_to_submit(&repo.jj, ["a", "b"], &pending).expect("changes to submit");
        let graph = BookmarkGraph::from_changes(&repo.jj, &changes, false).expect("graph");
        let submission_plan = plan(PlanContext {
            jj: &repo.jj,
            forge: &forge,
            config: &config,
            output: &output,
            bookmark_graph: &graph,
            dry_run: false,
        })
        .await
        .expect("plan");
        let existing = submission_plan.existing_mrs.clone();

        let result = execute(RootExecuteContext::new(
            &repo.jj,
            &forge,
            &config,
            &output,
            false,
            submission_plan,
            changes.clone(),
            false,
            false,
        ))
        .await
        .expect("execute");
        assert!(
            result.errors.is_empty(),
            "execute errors: {:?}",
            result.errors
        );
        assert_eq!(
            result.merge_requests.len(),
            1,
            "only the new PR gets an MR action: {:?}",
            result
                .merge_requests
                .iter()
                .map(|u| &u.bookmark)
                .collect::<Vec<_>>()
        );

        let runner = RecordingRunner::new(LinkOutcome::Linked);
        let outcome = link_stacks_with_runner(
            &runner,
            &github_config(),
            &repo.jj,
            &result,
            &existing,
            false,
            false,
            false,
        );

        let created: u64 = result.merge_requests[0]
            .mr
            .iid()
            .parse()
            .expect("numeric id");
        assert_eq!(
            runner.calls(),
            vec![vec![10, 20, created]],
            "whole stack linked"
        );
        assert!(
            matches!(&outcome, StackLinkOutcome::Linked { warnings, .. } if warnings.is_empty()),
            "unexpected outcome {outcome:?}"
        );
    }
}
