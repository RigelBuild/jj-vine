//! Register submitted GitHub pull requests as native stacks.

use core::cell::OnceCell;
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

/// Wall-clock budget for the whole optional stack-link phase, shared by the
/// jj commands that rebuild the bookmark graph, token resolution, and every
/// `gh-stack link` call in one submit.
const STACK_LINK_TIMEOUT: core::time::Duration = core::time::Duration::from_secs(60);

/// The only API URL whose token is handed to `gh-stack`. The helper talks to
/// github.com, so a GitHub Enterprise token must never reach it.
const GITHUB_COM_API_URL: &str = "https://api.github.com";

/// The `GH_HOST` set for `gh-stack`, so an inherited `GH_HOST` cannot route
/// the github.com token to another host.
const GITHUB_COM_HOST: &str = "github.com";

/// Result of trying to register submitted pull requests as GitHub stacks.
#[derive(Debug)]
pub enum StackLinkOutcome {
    /// No stack link was attempted, or there was nothing to link and nothing
    /// to warn about.
    Skipped(SkipReason),
    /// At least one stack was linked.
    Linked {
        /// Each linked stack as PR numbers, bottom to top.
        stacks: Vec<Vec<u64>>,
        /// Notes about submitted PRs left out of a linked stack: the top
        /// bookmarks above a linked run that have no PR yet, and PRs that are
        /// in no stack and not named in `warnings`.
        unlinked: Vec<String>,
        /// Problems with other stacks in the same run: a gap in a stack, a
        /// non-linear stack, `gh-stack` failing or exiting not-enabled, or
        /// `gh-stack` no longer installed before the remaining stacks.
        warnings: Vec<String>,
    },
    /// No stack was linked and at least one problem needs the user's
    /// attention. `warning` joins every problem with `"; "`. Submit itself
    /// remains successful.
    Failed {
        /// The user-facing description of every problem.
        warning: String,
    },
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
    /// A short user-facing description of why stack linking was skipped.
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
/// still takes its place in the stack. A stack that contains a bookmark in
/// `result.failed_bookmarks` is never linked: its pushed head or PR base may
/// not match the local stack.
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
    let runner = GhStackRunner::new(
        jj.cwd().to_path_buf(),
        std::env::var_os("PATH"),
        gh_extensions_dir(|name| std::env::var_os(name)),
        STACK_LINK_TIMEOUT,
    );
    let deadline = runner.deadline;
    link_stacks_with_runner(
        &runner,
        deadline,
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
    reason = "mirrors link_stacks plus the runner and deadline seams"
)]
fn link_stacks_with_runner(
    runner: &dyn StackLinkRunner,
    deadline: std::time::Instant,
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

    // The graph rebuild runs jj, so it shares the stack-link deadline: a slow
    // or hung jj command is killed at the deadline and only warns.
    let jj = jj.with_deadline(deadline);
    let graph = match BookmarkGraph::from_changes(&jj, &result.changes, tracked) {
        Ok(graph) => graph,
        Err(error) => {
            return StackLinkOutcome::Failed {
                warning: format!("could not rebuild bookmark graph for stack link: {error}"),
            };
        }
    };

    let pr_map = build_pr_map(existing_merge_requests, &result.merge_requests);
    link_from_graph(runner, github, &graph, &pr_map, &result.failed_bookmarks)
}

struct ContiguityScan {
    /// PRs from the bottom of the stack up to the first bookmark without one.
    prs: Vec<u64>,
    /// Bookmarks without a PR, in order, before any PR above them.
    unmapped: Vec<String>,
    /// The first bookmark without a PR that has a PR above it. Linking across
    /// it would rebase the PR above onto the wrong base.
    gap: Option<String>,
    /// PRs above `gap`, which stay unlinked.
    stranded: Vec<u64>,
}

fn scan_contiguous_run(
    ordered: &[BookmarkOrPending<'_>],
    pr_map: &BTreeMap<String, u64>,
) -> ContiguityScan {
    let mut scan = ContiguityScan {
        prs: Vec::new(),
        unmapped: Vec::new(),
        gap: None,
        stranded: Vec::new(),
    };

    for bookmark in ordered {
        match pr_map.get(bookmark.name()) {
            Some(&pr) if scan.gap.is_some() => scan.stranded.push(pr),
            Some(&pr) => match scan.unmapped.first() {
                // A missing bottom PR is a gap like a missing middle PR.
                Some(first) => {
                    scan.gap = Some(first.clone());
                    scan.stranded.push(pr);
                }
                None => scan.prs.push(pr),
            },
            None if scan.gap.is_none() => scan.unmapped.push(bookmark.name().to_owned()),
            None => {}
        }
    }

    scan
}

fn link_from_graph(
    runner: &dyn StackLinkRunner,
    github: &GitHubConfig,
    graph: &BookmarkGraph<'_>,
    pr_map: &BTreeMap<String, u64>,
    failed_bookmarks: &BTreeSet<String>,
) -> StackLinkOutcome {
    let mut stacks = Vec::new();
    let mut warnings = Vec::new();
    let mut notes = Vec::new();
    // PRs already named in a warning, so they are not listed again as orphans.
    let mut reported = BTreeSet::new();
    // Whether the runner has found gh-stack at least once in this run.
    let mut binary_found = false;

    for component in graph.components() {
        if !component.is_linear() {
            let mut prs: Vec<u64> = component
                .all_bookmarks()
                .iter()
                .filter_map(|bookmark| pr_map.get(bookmark.name()).copied())
                .collect();
            prs.sort_unstable();
            if prs.len() >= 2 {
                let warning = format!(
                    "skipped linking a non-linear stack; gh-stack links only linear stacks. Unlinked PRs: {}",
                    format_pr_list(&prs)
                );
                warn!("stack link: {warning}");
                warnings.push(warning);
                reported.extend(prs);
            } else {
                debug!("stack link: skipping a non-linear component without PRs to link");
            }
            continue;
        }
        let Some(leaf) = component.leaves.first() else {
            continue;
        };
        let mut ordered = leaf.downstack();
        ordered.reverse();
        // A failed push or PR update leaves the remote out of step with the
        // local stack, so the plan-time PR map is stale for this component.
        let failed: Vec<&str> = ordered
            .iter()
            .map(BookmarkOrPending::name)
            .filter(|name| failed_bookmarks.contains(*name))
            .collect();
        if !failed.is_empty() {
            let prs: Vec<u64> = ordered
                .iter()
                .filter_map(|bookmark| pr_map.get(bookmark.name()).copied())
                .collect();
            if prs.is_empty() {
                debug!("stack link: skipping a failed component without PRs");
                continue;
            }
            let warning = format!(
                "skipped linking a stack because submitting {} failed; unlinked PRs: {}",
                failed.join(", "),
                format_prs(&prs)
            );
            warn!("stack link: {warning}");
            warnings.push(warning);
            reported.extend(prs);
            continue;
        }
        let ContiguityScan {
            prs,
            unmapped,
            gap,
            stranded,
        } = scan_contiguous_run(&ordered, pr_map);

        if let Some(gap) = gap {
            let warning = if prs.is_empty() {
                format!(
                    "skipped linking a stack whose bottom bookmark {gap} has no PR; PRs above it: {}",
                    format_prs(&stranded)
                )
            } else {
                format!(
                    "skipped linking a stack with a missing middle PR ({gap}); PRs below the gap: {}; PRs above the gap: {}",
                    format_prs(&prs),
                    format_prs(&stranded)
                )
            };
            warn!("stack link: {warning}");
            warnings.push(warning);
            reported.extend(prs.iter().chain(&stranded).copied());
            continue;
        }
        if prs.len() < 2 {
            debug!("stack link: no qualifying PR stack in component");
            continue;
        }
        // Without a gap, every unmapped bookmark sits above the linked run.
        let trailing_unmapped = unmapped;

        let outcome = runner.run(&prs, github);
        binary_found |= outcome != LinkOutcome::MissingBinary;
        match outcome {
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
            LinkOutcome::UnsupportedHost => {
                warnings.push(
                    "gh-stack links stacks only on github.com, but github.host is not https://api.github.com; set github.linkStack = false for this repository"
                        .to_owned(),
                );
                break;
            }
            LinkOutcome::NotEnabled => {
                warnings.push(format!(
                    "stacked PRs are not enabled for {}; enable them or set github.linkStack = false",
                    github.target_project()
                ));
                reported.extend(prs.iter().copied());
            }
            LinkOutcome::MissingBinary => {
                if !binary_found {
                    if stacks.is_empty() && warnings.is_empty() {
                        return StackLinkOutcome::Skipped(SkipReason::MissingBinary);
                    }
                    warnings.push(
                        "gh-stack is not installed; remaining stacks were not linked".to_owned(),
                    );
                } else {
                    warnings.push(
                        "gh-stack is no longer installed; remaining stacks were not linked"
                            .to_owned(),
                    );
                }
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

/// Format PRs that have no stack order.
fn format_pr_list(prs: &[u64]) -> String {
    prs.iter()
        .map(|pr| format!("#{pr}"))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum LinkOutcome {
    Linked,
    NotEnabled,
    MissingBinary,
    /// `github.host` is not github.com, so `gh-stack` was not run.
    UnsupportedHost,
    Failed {
        detail: String,
    },
}

trait StackLinkRunner {
    fn run(&self, prs: &[u64], github: &GitHubConfig) -> LinkOutcome;
}

/// The GitHub CLI extensions directory, resolved the way `gh` resolves its
/// data directory: `GH_DATA_DIR`, then `XDG_DATA_HOME/gh`, then (on Windows)
/// `LOCALAPPDATA/GitHub CLI`, then `HOME/.local/share/gh`.
fn gh_extensions_dir(var: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let non_empty = |name: &str| var(name).filter(|value| !value.is_empty());
    let data_dir = if let Some(dir) = non_empty("GH_DATA_DIR") {
        PathBuf::from(dir)
    } else if let Some(dir) = non_empty("XDG_DATA_HOME") {
        PathBuf::from(dir).join("gh")
    } else if let Some(dir) = non_empty("LOCALAPPDATA").filter(|_| cfg!(windows)) {
        PathBuf::from(dir).join("GitHub CLI")
    } else {
        PathBuf::from(non_empty("HOME")?).join(".local/share/gh")
    };
    Some(data_dir.join("extensions"))
}

struct GhStackRunner {
    cwd: PathBuf,
    /// Where `gh-stack` is searched for, in order: the `PATH` directories,
    /// then the directory a `gh extension install` of `gh stack` uses.
    /// Relative entries are resolved against `cwd`, where `gh-stack` runs.
    search_dirs: Vec<PathBuf>,
    /// The stack-link budget, reported in timeout warnings.
    timeout: core::time::Duration,
    /// When the budget runs out. Token resolution and every `gh-stack link`
    /// call share it.
    deadline: std::time::Instant,
    /// The token, or the failure detail, resolved once on first use so a
    /// `tokenCommand` runs at most once per stack-link phase.
    token: OnceCell<Result<String, String>>,
}

impl GhStackRunner {
    /// Start the stack-link phase: the `timeout` budget starts now.
    fn new(
        cwd: PathBuf,
        search_path: Option<OsString>,
        extensions_dir: Option<PathBuf>,
        timeout: core::time::Duration,
    ) -> Self {
        // `gh-stack` runs in `cwd`, so relative entries resolve there.
        let base = std::path::absolute(&cwd).unwrap_or_else(|_| cwd.clone());
        let search_dirs = search_path
            .map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
            .unwrap_or_default()
            .into_iter()
            .chain(extensions_dir.map(|dir| dir.join("gh-stack")))
            .map(|dir| base.join(dir))
            .collect();
        Self {
            cwd,
            search_dirs,
            timeout,
            deadline: std::time::Instant::now() + timeout,
            token: OnceCell::new(),
        }
    }

    /// The first executable `gh-stack` in `search_dirs`, as a path that stays
    /// valid inside `cwd`.
    fn find_binary(&self) -> Option<PathBuf> {
        self.search_dirs
            .iter()
            .find_map(|dir| which::which(dir.join("gh-stack")).ok())
    }

    /// Time left in the stack-link budget, or `None` once it has run out.
    fn remaining(&self) -> Option<core::time::Duration> {
        let remaining = self
            .deadline
            .saturating_duration_since(std::time::Instant::now());
        (!remaining.is_zero()).then_some(remaining)
    }

    fn deadline_passed(&self) -> LinkOutcome {
        LinkOutcome::Failed {
            detail: format!(
                "the {:?} stack-link deadline passed before gh-stack ran",
                self.timeout
            ),
        }
    }

    fn run_with_binary(&self, binary: &Path, prs: &[u64], github: &GitHubConfig) -> LinkOutcome {
        // A token command must not start, or run, past the shared deadline.
        let Some(remaining) = self.remaining() else {
            return self.deadline_passed();
        };
        let token = match self.token.get_or_init(|| {
            github
                .resolved_token_with_timeout(remaining.min(crate::config::TOKEN_COMMAND_TIMEOUT))
                .map_err(|error| format!("could not resolve GH_TOKEN: {error}"))
        }) {
            Ok(token) => token,
            Err(detail) => {
                return LinkOutcome::Failed {
                    detail: detail.clone(),
                };
            }
        };

        let Some(remaining) = self.remaining() else {
            return self.deadline_passed();
        };

        let mut command = std::process::Command::new(binary);
        command.arg("link");
        for pr in prs {
            command.arg(pr.to_string());
        }
        command.current_dir(&self.cwd);
        command.env("GH_TOKEN", token);
        command.env("GH_HOST", GITHUB_COM_HOST);
        command.env("GH_REPO", github.target_project());

        match crate::process::output_with_timeout(command, remaining) {
            Ok(Some(output)) if output.status.success() => LinkOutcome::Linked,
            Ok(Some(output)) if output.status.code() == Some(9) => LinkOutcome::NotEnabled,
            Ok(Some(output)) => {
                let stderr = String::from_utf8_lossy(&output.stderr);
                let stderr = stderr.replace(token.as_str(), "[redacted]");
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
                detail: format!(
                    "gh-stack did not finish within the {:?} stack-link deadline",
                    self.timeout
                ),
            },
            // The binary was found, so a spawn failure here (including
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
        let Some(binary) = self.find_binary() else {
            return LinkOutcome::MissingBinary;
        };
        if github.host.trim_end_matches('/') != GITHUB_COM_API_URL {
            return LinkOutcome::UnsupportedHost;
        }
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
            host: GITHUB_COM_API_URL.to_owned(),
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

    /// A stack-link deadline no test reaches.
    fn far_deadline() -> std::time::Instant {
        std::time::Instant::now() + STACK_LINK_TIMEOUT
    }

    #[test]
    fn binary_absent_from_path_is_a_silent_skip() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let runner = GhStackRunner::new(
            temp.path().to_path_buf(),
            Some(temp.path().as_os_str().to_owned()),
            None,
            STACK_LINK_TIMEOUT,
        );

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
        let runner = GhStackRunner::new(temp.path().to_path_buf(), None, None, STACK_LINK_TIMEOUT);
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
            &BTreeSet::new(),
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
            &BTreeSet::new(),
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
            &BTreeSet::new(),
        );

        assert!(runner.calls().is_empty());
        assert!(
            matches!(&outcome, StackLinkOutcome::Failed { warning }
                if warning.contains("missing middle PR (b)")
                    && warning.contains("PRs below the gap: #10")
                    && warning.contains("PRs above the gap: #30")),
            "every unlinked PR must be named, got {outcome:?}"
        );
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
            &BTreeSet::new(),
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
            &BTreeSet::new(),
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
            failed_bookmarks: BTreeSet::new(),
        };
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        assert!(matches!(
            link_stacks_with_runner(
                &runner,
                far_deadline(),
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
                far_deadline(),
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
            failed_bookmarks: BTreeSet::new(),
        };
        let runner = RecordingRunner::new(LinkOutcome::Linked);
        let mut github = github_config();
        github.link_stack = false;

        assert!(matches!(
            link_stacks_with_runner(
                &runner,
                far_deadline(),
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
            &BTreeSet::new(),
        );

        assert_eq!(runner.calls(), vec![vec![10, 20, 30]], "whole stack linked");
        assert!(
            matches!(&outcome, StackLinkOutcome::Linked { stacks, .. } if *stacks == vec![vec![10, 20, 30]]),
            "unexpected outcome {outcome:?}"
        );
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
            &BTreeSet::new(),
        );

        let mut calls = runner.calls();
        calls.sort();
        assert_eq!(calls, vec![vec![1, 2], vec![3, 4]]);
        assert!(matches!(outcome, StackLinkOutcome::Linked { .. }));
    }

    #[test]
    fn non_linear_component_warns_and_names_its_pull_requests() {
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
            &BTreeSet::new(),
        );

        assert!(
            runner.calls().is_empty(),
            "gh-stack links only linear stacks"
        );
        assert!(
            matches!(&outcome, StackLinkOutcome::Failed { warning }
                if warning.contains("non-linear") && warning.contains("#1, #2, #3")),
            "a skipped non-linear stack must be reported, got {outcome:?}"
        );
    }

    #[test]
    fn non_linear_component_is_not_repeated_as_orphans_beside_a_linked_stack() {
        let mut changes = Change::mock_stack_map([
            Change::mock_from_bookmark("a"),
            Change::mock_from_bookmark("b"),
            Change::mock_from_bookmark("c").with_mock_parent_bookmarks(["a", "b"]),
        ]);
        changes.extend(linear_changes(&["x", "y"]));
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[
                mr_update("a", "1"),
                mr_update("b", "2"),
                mr_update("c", "3"),
                mr_update("x", "7"),
                mr_update("y", "8"),
            ]),
            &BTreeSet::new(),
        );

        let StackLinkOutcome::Linked {
            stacks,
            unlinked,
            warnings,
        } = &outcome
        else {
            panic!("the linear stack must still link, got {outcome:?}");
        };
        assert_eq!(*stacks, vec![vec![7, 8]]);
        assert!(
            warnings.len() == 1 && warnings[0].contains("non-linear"),
            "non-linear warning kept: {warnings:?}"
        );
        assert!(
            unlinked.is_empty(),
            "no duplicate orphan note: {unlinked:?}"
        );
    }

    #[test]
    fn missing_bottom_pr_never_links_the_prs_above_it() {
        let changes = linear_changes(&["a", "b", "c"]);
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[mr_update("b", "20"), mr_update("c", "30")]),
            &BTreeSet::new(),
        );

        assert!(
            runner.calls().is_empty(),
            "linking #20 -> #30 would rebase #20 off the missing bottom PR"
        );
        assert!(
            matches!(&outcome, StackLinkOutcome::Failed { warning }
                if warning.contains("bottom bookmark a has no PR")
                    && warning.contains("#20 -> #30")),
            "unexpected outcome {outcome:?}"
        );
    }

    #[test]
    fn missing_bottom_pr_does_not_block_other_stacks() {
        let mut changes = linear_changes(&["a", "b", "c"]);
        changes.extend(linear_changes(&["x", "y"]));
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[
                mr_update("b", "20"),
                mr_update("c", "30"),
                mr_update("x", "7"),
                mr_update("y", "8"),
            ]),
            &BTreeSet::new(),
        );

        assert_eq!(runner.calls(), vec![vec![7, 8]]);
        let StackLinkOutcome::Linked {
            unlinked, warnings, ..
        } = &outcome
        else {
            panic!("the complete stack must still link, got {outcome:?}");
        };
        assert!(
            warnings.len() == 1 && warnings[0].contains("bottom bookmark a"),
            "bottom gap warned: {warnings:?}"
        );
        assert!(
            unlinked.is_empty(),
            "PRs named in the gap warning are not repeated: {unlinked:?}"
        );
    }

    #[test]
    fn a_second_missing_pr_above_an_interior_gap_still_blocks_linking() {
        let changes = linear_changes(&["a", "b", "c", "d", "e"]);
        let runner = RecordingRunner::new(LinkOutcome::Linked);

        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[
                mr_update("a", "1"),
                mr_update("c", "3"),
                mr_update("e", "5"),
            ]),
            &BTreeSet::new(),
        );

        assert!(runner.calls().is_empty());
        assert!(
            matches!(&outcome, StackLinkOutcome::Failed { warning }
                if warning.contains("missing middle PR (b)")
                    && warning.contains("PRs below the gap: #1;")
                    && warning.contains("PRs above the gap: #3 -> #5")),
            "the first gap is named with every PR above it, got {outcome:?}"
        );
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
            &BTreeSet::new(),
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
            &BTreeSet::new(),
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
            &BTreeSet::new(),
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
            &BTreeSet::new(),
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
            &BTreeSet::new(),
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
            &BTreeSet::new(),
        );

        assert_eq!(
            runner.calls.borrow().clone(),
            vec![vec![1, 2], vec![3, 4]],
            "components run in bookmark order"
        );
        let StackLinkOutcome::Linked {
            stacks,
            unlinked,
            warnings,
        } = outcome
        else {
            panic!("a successful component must survive a later failure, got {outcome:?}");
        };
        assert_eq!(stacks, vec![vec![1, 2]], "successful stack ids kept");
        assert_eq!(warnings.len(), 1, "one warning for the failed component");
        assert!(
            warnings[0].contains("network unreachable")
                && warnings[0].contains("gh-stack link 3 4"),
            "failure warning keeps rerun command: {warnings:?}"
        );
        assert!(
            unlinked.iter().any(|note| note.contains("#9")),
            "standalone PR is still reported: {unlinked:?}"
        );
        assert!(
            !unlinked
                .iter()
                .any(|note| note.contains("#3") || note.contains("#4")),
            "PRs named in a warning are not repeated as orphans: {unlinked:?}"
        );
        assert!(
            unlinked.iter().any(|note| note.contains("top")),
            "trailing-bookmark note kept: {unlinked:?}"
        );
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
            &BTreeSet::new(),
        );

        assert!(
            matches!(&outcome, StackLinkOutcome::Linked { stacks, warnings, .. }
                if stacks.len() == 1 && warnings.len() == 1 && warnings[0].contains("no longer installed")),
            "unexpected outcome {outcome:?}"
        );
    }

    #[test]
    fn binary_missing_after_an_earlier_warning_says_not_installed() {
        let mut changes = linear_changes(&["a", "b", "c"]);
        changes.extend(linear_changes(&["x", "y"]));
        // The gapped stack warns without running gh-stack; the first runner
        // call then finds no binary at all.
        let runner = SequencedRunner::new([LinkOutcome::MissingBinary]);

        let outcome = link_from_graph(
            &runner,
            &github_config(),
            &graph(&changes),
            &pr_map(&[
                mr_update("a", "1"),
                mr_update("c", "3"),
                mr_update("x", "7"),
                mr_update("y", "8"),
            ]),
            &BTreeSet::new(),
        );

        assert!(
            matches!(&outcome, StackLinkOutcome::Failed { warning }
                if warning.contains("gh-stack is not installed")
                    && !warning.contains("no longer on PATH")),
            "gh-stack was never found, so it did not leave PATH: {outcome:?}"
        );
    }

    /// Write an executable `gh-stack` shell script into `dir`.
    #[cfg(unix)]
    fn fake_gh_stack(dir: &Path, script: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;

        let binary = dir.join("gh-stack");
        std::fs::write(&binary, format!("#!/bin/sh\n{script}\n")).expect("write helper");
        let mut permissions = std::fs::metadata(&binary)
            .expect("helper metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&binary, permissions).expect("make helper executable");
        binary
    }

    #[cfg(unix)]
    #[test]
    fn stderr_does_not_expose_github_token() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let binary = fake_gh_stack(temp.path(), "printf '%s\\n' \"$GH_TOKEN\" >&2\nexit 1");

        let outcome = GhStackRunner::new(temp.path().to_path_buf(), None, None, STACK_LINK_TIMEOUT)
            .run_with_binary(&binary, &[1, 2], &github_config());

        assert!(matches!(
            outcome,
            LinkOutcome::Failed { detail }
                if detail.contains("[redacted]") && !detail.contains("test-token")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn exit_nine_from_gh_stack_on_path_means_not_enabled() {
        let temp = tempfile::tempdir().expect("temporary directory");
        fake_gh_stack(temp.path(), "echo 'stacks are not enabled' >&2\nexit 9");
        let runner = GhStackRunner::new(
            temp.path().to_path_buf(),
            Some(temp.path().as_os_str().to_owned()),
            None,
            STACK_LINK_TIMEOUT,
        );

        let outcome = runner.run(&[1, 2], &github_config());

        assert_eq!(outcome, LinkOutcome::NotEnabled);
    }

    #[cfg(unix)]
    #[test]
    fn gh_stack_receives_link_args_token_and_repo() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let seen = temp.path().join("seen");
        fake_gh_stack(
            temp.path(),
            &format!(
                "printf '%s|%s|%s|%s' \"$*\" \"$GH_TOKEN\" \"$GH_HOST\" \"$GH_REPO\" > '{}'",
                seen.display()
            ),
        );
        let runner = GhStackRunner::new(
            temp.path().to_path_buf(),
            Some(temp.path().as_os_str().to_owned()),
            None,
            STACK_LINK_TIMEOUT,
        );

        let outcome = runner.run(&[10, 20, 30], &github_config());

        assert_eq!(outcome, LinkOutcome::Linked);
        assert_eq!(
            std::fs::read_to_string(&seen).expect("helper recorded its inputs"),
            "link 10 20 30|test-token|github.com|owner/repo"
        );
    }

    #[cfg(unix)]
    #[test]
    fn overrunning_gh_stack_times_out_as_a_failure() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let binary = fake_gh_stack(temp.path(), "exec sleep 30");
        let timeout = core::time::Duration::from_millis(200);
        let runner = GhStackRunner::new(temp.path().to_path_buf(), None, None, timeout);

        let start = std::time::Instant::now();
        let outcome = runner.run_with_binary(&binary, &[1, 2], &github_config());

        assert!(
            matches!(&outcome, LinkOutcome::Failed { detail } if detail.contains("within the 200ms stack-link deadline")),
            "a timeout must warn, got {outcome:?}"
        );
        assert!(
            start.elapsed() < core::time::Duration::from_secs(5),
            "must return promptly after the timeout, not wait out the sleep"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stack_link_deadline_is_shared_across_components() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let calls = temp.path().join("calls");
        // Each call is recorded, then sleeps past the whole budget.
        fake_gh_stack(
            temp.path(),
            &format!("echo \"$*\" >> '{}'\nexec sleep 30", calls.display()),
        );
        let timeout = core::time::Duration::from_millis(300);
        let runner = GhStackRunner::new(
            temp.path().to_path_buf(),
            Some(temp.path().as_os_str().to_owned()),
            None,
            timeout,
        );
        let mut changes = linear_changes(&["a", "b"]);
        changes.extend(linear_changes(&["c", "d"]));

        let start = std::time::Instant::now();
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
            &BTreeSet::new(),
        );

        assert!(
            start.elapsed() < core::time::Duration::from_secs(10),
            "must not wait out the helper's sleep, took {:?}",
            start.elapsed()
        );
        assert_eq!(
            std::fs::read_to_string(&calls)
                .expect("first component ran gh-stack")
                .lines()
                .count(),
            1,
            "the second component must not start a fresh budget"
        );
        assert!(
            matches!(&outcome, StackLinkOutcome::Failed { warning }
                if warning.contains("stack-link deadline")
                    && warning.contains("#3 -> #4")
                    && !warning.contains("test-token")),
            "both components must warn without the token, got {outcome:?}"
        );
    }

    /// Whether `pid` names a process that exists and is not a zombie.
    #[cfg(unix)]
    fn is_running(pid: &str) -> bool {
        let output = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .stderr(std::process::Stdio::null())
            .output()
            .expect("ps must run");
        let state = String::from_utf8_lossy(&output.stdout);
        let state = state.trim();
        !state.is_empty() && !state.starts_with('Z')
    }

    #[cfg(unix)]
    #[test]
    fn hung_jj_during_graph_rebuild_warns_at_the_shared_deadline() {
        const CHILD_MODE: &str = "JJ_VINE_TEST_HUNG_GRAPH_JJ";
        const REPO_DIR: &str = "JJ_VINE_TEST_REPO_DIR";
        if std::env::var_os(CHILD_MODE).is_some() {
            let repo = std::env::var_os(REPO_DIR).expect("repository directory");
            let jj = Jujutsu::new(PathBuf::from(repo)).expect("fake jj on PATH");
            let result = SubmissionResult {
                merge_requests: vec![mr_update("a", "1"), mr_update("b", "2")],
                errors: vec![],
                bookmarks_pushed: vec![],
                changes: linear_changes(&["a", "b"])
                    .into_iter()
                    .map(|(_, change)| change)
                    .collect(),
                failed_bookmarks: BTreeSet::new(),
            };
            let runner = RecordingRunner::new(LinkOutcome::Linked);
            let start = std::time::Instant::now();
            let outcome = link_stacks_with_runner(
                &runner,
                start + core::time::Duration::from_millis(300),
                &github_config(),
                &jj,
                &result,
                &HashMap::new(),
                false,
                false,
                false,
            );
            assert!(
                start.elapsed() < core::time::Duration::from_secs(5),
                "a hung jj must not hold the phase past its deadline, took {:?}",
                start.elapsed()
            );
            assert!(
                matches!(&outcome, StackLinkOutcome::Failed { warning }
                    if warning.contains("could not rebuild bookmark graph")
                        && warning.contains("deadline")),
                "a hung graph rebuild must warn, got {outcome:?}"
            );
            assert!(runner.calls().is_empty(), "gh-stack must not run");
            return;
        }

        let temp = tempfile::tempdir().expect("temporary directory");
        let bin = temp.path().join("bin");
        std::fs::create_dir_all(&bin).expect("bin directory");
        // `fake_gh_stack` writes an executable script; rename it to `jj`. The
        // script reads the pid directory from the environment, so no path is
        // spliced into shell source.
        let script = fake_gh_stack(
            &bin,
            "echo $$ > \"$JJ_VINE_TEST_PID_DIR/jj.pid\"\n\
             sh -c 'echo $$ > \"$JJ_VINE_TEST_PID_DIR/descendant.pid\"; exec sleep 30' &\n\
             exec sleep 30",
        );
        std::fs::rename(script, bin.join("jj")).expect("install fake jj");
        let mut path = bin.as_os_str().to_owned();
        path.push(":");
        path.push(std::env::var_os("PATH").expect("PATH is set"));

        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "submit::stack_link::tests::hung_jj_during_graph_rebuild_warns_at_the_shared_deadline",
            ])
            .env(CHILD_MODE, "1")
            .env(REPO_DIR, temp.path())
            .env("JJ_VINE_TEST_PID_DIR", temp.path())
            .env("PATH", path)
            .output()
            .expect("run child test");
        assert!(
            output.status.success(),
            "child test failed: {}",
            String::from_utf8_lossy(&output.stdout),
        );
        for name in ["jj.pid", "descendant.pid"] {
            let pid = std::fs::read_to_string(temp.path().join(name)).expect("fake jj ran");
            let start = std::time::Instant::now();
            while is_running(pid.trim()) && start.elapsed() < core::time::Duration::from_secs(5) {
                std::thread::sleep(core::time::Duration::from_millis(20));
            }
            assert!(
                !is_running(pid.trim()),
                "{name}: the graph rebuild must not leave a jj process running"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_github_com_host_never_runs_the_helper() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let seen = temp.path().join("seen");
        fake_gh_stack(
            temp.path(),
            &format!("printf '%s' \"$GH_TOKEN\" > '{}'", seen.display()),
        );
        let runner = GhStackRunner::new(
            temp.path().to_path_buf(),
            Some(temp.path().as_os_str().to_owned()),
            None,
            STACK_LINK_TIMEOUT,
        );
        let counter = temp.path().join("token-calls");
        let github = GitHubConfig {
            host: "https://github.example.com/api/v3".to_owned(),
            token: String::new(),
            token_command: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                format!("echo call >> '{}'; printf ghe-token", counter.display()),
            ],
            ..github_config()
        };
        let changes = linear_changes(&["a", "b"]);

        let outcome = link_from_graph(
            &runner,
            &github,
            &graph(&changes),
            &pr_map(&[mr_update("a", "1"), mr_update("b", "2")]),
            &BTreeSet::new(),
        );

        assert!(
            !seen.exists(),
            "gh-stack must not run for a non-github.com host"
        );
        assert!(!counter.exists(), "the token must not be resolved");
        assert!(
            matches!(&outcome, StackLinkOutcome::Failed { warning }
                if warning.contains("github.linkStack = false")
                    && !warning.contains("ghe-token")),
            "a non-github.com host must warn, got {outcome:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn relative_path_entry_resolves_against_the_command_cwd() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let bin = temp.path().join("bin");
        std::fs::create_dir(&bin).expect("bin directory");
        fake_gh_stack(&bin, "exit 0");
        let runner = GhStackRunner::new(
            temp.path().to_path_buf(),
            Some(OsString::from("bin")),
            None,
            STACK_LINK_TIMEOUT,
        );

        assert_eq!(runner.run(&[1, 2], &github_config()), LinkOutcome::Linked);
    }

    #[test]
    fn extensions_dir_follows_gh_data_dir_precedence() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                vars.iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| OsString::from(value))
            }
        };

        assert_eq!(
            gh_extensions_dir(env(&[
                ("GH_DATA_DIR", "/gh-data"),
                ("XDG_DATA_HOME", "/xdg"),
                ("HOME", "/home/u"),
            ])),
            Some(PathBuf::from("/gh-data/extensions"))
        );
        assert_eq!(
            gh_extensions_dir(env(&[
                ("GH_DATA_DIR", ""),
                ("XDG_DATA_HOME", "/xdg"),
                ("HOME", "/home/u"),
            ])),
            Some(PathBuf::from("/xdg/gh/extensions"))
        );
        #[cfg(not(windows))]
        assert_eq!(
            gh_extensions_dir(env(&[("LOCALAPPDATA", "/local"), ("HOME", "/home/u")])),
            Some(PathBuf::from("/home/u/.local/share/gh/extensions"))
        );
        assert_eq!(gh_extensions_dir(env(&[])), None);
    }

    #[cfg(unix)]
    #[test]
    fn installed_gh_extension_is_found_without_path_entry() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let extensions = temp.path().join("extensions");
        let extension = extensions.join("gh-stack");
        std::fs::create_dir_all(&extension).expect("extension directory");
        let seen = temp.path().join("seen");
        fake_gh_stack(
            &extension,
            &format!("printf '%s' \"$*\" > '{}'", seen.display()),
        );
        let runner = GhStackRunner::new(
            temp.path().to_path_buf(),
            Some(temp.path().as_os_str().to_owned()),
            Some(extensions),
            STACK_LINK_TIMEOUT,
        );

        assert_eq!(runner.run(&[1, 2], &github_config()), LinkOutcome::Linked);
        assert_eq!(
            std::fs::read_to_string(&seen).expect("extension ran"),
            "link 1 2"
        );
    }

    #[test]
    fn missing_extension_and_path_binary_is_a_silent_skip() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let extensions = temp.path().join("extensions");
        std::fs::create_dir_all(extensions.join("gh-other")).expect("other extension");
        let runner = GhStackRunner::new(
            temp.path().to_path_buf(),
            Some(temp.path().as_os_str().to_owned()),
            Some(extensions),
            STACK_LINK_TIMEOUT,
        );

        assert_eq!(
            runner.run(&[1, 2], &github_config()),
            LinkOutcome::MissingBinary
        );
    }

    #[cfg(unix)]
    #[test]
    fn expired_deadline_never_starts_the_token_command() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let binary = fake_gh_stack(temp.path(), "exit 0");
        let counter = temp.path().join("token-calls");
        let github = GitHubConfig {
            token: String::new(),
            token_command: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                format!("echo call >> '{}'; printf command-token", counter.display()),
            ],
            ..github_config()
        };
        let runner = GhStackRunner::new(
            temp.path().to_path_buf(),
            None,
            None,
            core::time::Duration::ZERO,
        );

        let outcome = runner.run_with_binary(&binary, &[1, 2], &github);

        assert!(
            matches!(&outcome, LinkOutcome::Failed { detail } if detail.contains("stack-link deadline passed")),
            "an expired budget must warn, got {outcome:?}"
        );
        assert!(!counter.exists(), "the token command must not start");
    }

    #[cfg(unix)]
    #[test]
    fn token_command_is_bounded_by_the_remaining_deadline() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let binary = fake_gh_stack(temp.path(), "exit 0");
        let github = GitHubConfig {
            token: String::new(),
            token_command: vec!["sh".to_owned(), "-c".to_owned(), "exec sleep 30".to_owned()],
            ..github_config()
        };
        let runner = GhStackRunner::new(
            temp.path().to_path_buf(),
            None,
            None,
            core::time::Duration::from_millis(300),
        );

        let start = std::time::Instant::now();
        let outcome = runner.run_with_binary(&binary, &[1, 2], &github);

        assert!(
            start.elapsed() < core::time::Duration::from_secs(5),
            "the token command must stop at the stack-link deadline, took {:?}",
            start.elapsed()
        );
        assert!(
            matches!(&outcome, LinkOutcome::Failed { detail } if detail.contains("could not resolve GH_TOKEN")),
            "a timed-out token command must warn, got {outcome:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn token_command_runs_once_per_stack_link_phase() {
        let temp = tempfile::tempdir().expect("temporary directory");
        let binary = fake_gh_stack(temp.path(), "exit 0");
        let counter = temp.path().join("token-calls");
        let github = GitHubConfig {
            token: String::new(),
            token_command: vec![
                "sh".to_owned(),
                "-c".to_owned(),
                format!("echo call >> '{}'; printf command-token", counter.display()),
            ],
            ..github_config()
        };
        let runner = GhStackRunner::new(temp.path().to_path_buf(), None, None, STACK_LINK_TIMEOUT);

        assert_eq!(
            runner.run_with_binary(&binary, &[1, 2], &github),
            LinkOutcome::Linked
        );
        assert_eq!(
            runner.run_with_binary(&binary, &[3, 4], &github),
            LinkOutcome::Linked
        );

        assert_eq!(
            std::fs::read_to_string(&counter)
                .expect("token command ran")
                .lines()
                .count(),
            1,
            "one tokenCommand run for two stacks"
        );
    }

    /// Run plan → execute → stack link over a pushed `a → b → c` stack whose
    /// PRs #10, #20, #30 already exist. `b_target` is the base of #20 on the
    /// forge. Returns the execute errors and the stack-link outcome with the
    /// runner calls.
    async fn production_link_with_existing_prs(
        b_target: &str,
        forge: crate::forge::test::TestForge,
        push: crate::config::RepoPushConfig,
    ) -> (
        Vec<crate::error::ClonableError>,
        StackLinkOutcome,
        Vec<Vec<u64>>,
    ) {
        use std::collections::HashSet;

        use crate::{
            config::{Config, ForgeType},
            forge::ForgeImpl,
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
        repo.create_change("c.txt", "c", "C").create_bookmark("c");
        repo.push_bookmark("c");

        let mut forge = forge;
        for (id, source, target) in [("10", "a", "main"), ("20", "b", b_target), ("30", "c", "b")] {
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
            .github(github_config())
            .push(push)
            .build();
        let output = BufferedOutput::new();

        let changes = find_changes_to_submit(&repo.jj, ["c"], &HashSet::<String>::new())
            .expect("changes to submit");
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
        assert_eq!(existing.len(), 3, "every PR is found at planning time");

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

        let runner = RecordingRunner::new(LinkOutcome::Linked);
        let outcome = link_stacks_with_runner(
            &runner,
            far_deadline(),
            &github_config(),
            &repo.jj,
            &result,
            &existing,
            false,
            false,
            false,
        );
        (result.errors, outcome, runner.calls())
    }

    /// A failed push leaves every remote head stale, and the MR actions that
    /// depend on it are skipped. The plan-time PRs must not be linked.
    #[tokio::test]
    async fn production_failed_push_leaves_stack_unlinked() {
        let (errors, outcome, calls) = production_link_with_existing_prs(
            "a",
            crate::forge::test::TestForge::default(),
            crate::config::RepoPushConfig::Command(vec!["false".to_owned()]),
        )
        .await;

        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("Failed to push")),
            "the push must fail in execute: {errors:?}"
        );
        assert!(calls.is_empty(), "stale remote heads must not be linked");
        assert!(
            matches!(&outcome, StackLinkOutcome::Failed { warning }
                if warning.contains("submitting a, b, c failed")
                    && warning.contains("#10 -> #20 -> #30")),
            "unexpected outcome {outcome:?}"
        );
    }

    /// #20 targets `main` but must target `a`. When that base update fails,
    /// #20 still has the wrong base, so the stack must not be linked.
    #[tokio::test]
    async fn production_failed_base_update_leaves_stack_unlinked() {
        let (errors, outcome, calls) = production_link_with_existing_prs(
            "main",
            crate::forge::test::TestForge::builder()
                .fail_update_base_for(std::collections::HashSet::from(["b".to_owned()]))
                .build(),
            crate::config::RepoPushConfig::default(),
        )
        .await;

        assert!(
            errors
                .iter()
                .any(|error| error.message.contains("Failed to update MR base for b")),
            "the base update must fail in execute: {errors:?}"
        );
        assert!(
            calls.is_empty(),
            "a PR with a stale base must not be linked"
        );
        assert!(
            matches!(&outcome, StackLinkOutcome::Failed { warning }
                if warning.contains("submitting b failed")
                    && warning.contains("#10 -> #20 -> #30")),
            "unexpected outcome {outcome:?}"
        );
    }

    /// Drive plan → execute → stack link where creating the bottom PR fails.
    /// The PRs above it already exist, so execute returns a partial error and
    /// a PR map with no bottom entry. Linking them would rebase the lowest PR
    /// onto trunk, so the stack must stay unlinked.
    #[tokio::test]
    async fn production_failed_bottom_pr_create_leaves_stack_unlinked() {
        use std::collections::HashSet;

        use crate::{
            config::{Config, ForgeType},
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
        repo.create_change("c.txt", "c", "C").create_bookmark("c");
        repo.push_bookmark("c");

        let mut forge = TestForge::builder()
            .fail_create_for(HashSet::from(["a".to_owned()]))
            .build();
        for (id, source, target) in [("20", "b", "a"), ("30", "c", "b")] {
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
            .github(github_config())
            .build();
        let output = BufferedOutput::new();

        let changes = find_changes_to_submit(&repo.jj, ["c"], &HashSet::<String>::new())
            .expect("changes to submit");
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
            !result.errors.is_empty(),
            "the bottom PR create must fail in execute"
        );
        assert!(
            !result
                .merge_requests
                .iter()
                .any(|update| update.bookmark == "a"),
            "no PR for the bottom bookmark"
        );

        let runner = RecordingRunner::new(LinkOutcome::Linked);
        let outcome = link_stacks_with_runner(
            &runner,
            far_deadline(),
            &github_config(),
            &repo.jj,
            &result,
            &existing,
            false,
            false,
            false,
        );

        assert!(
            runner.calls().is_empty(),
            "#20 -> #30 must not be linked without the bottom PR"
        );
        assert!(
            matches!(&outcome, StackLinkOutcome::Failed { warning }
                if warning.contains("bottom bookmark a has no PR")
                    && warning.contains("#20 -> #30")),
            "unexpected outcome {outcome:?}"
        );
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
            far_deadline(),
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
