use std::collections::HashMap;

use regex::Regex;
use snafu::whatever;

use crate::{
    bookmark::{BookmarkGraph, JJName as _},
    config::{Config, SpecImpactCheckConfig, SubmitChecksConfig},
    error::{ConfigSnafu, Result},
    jj::Jujutsu,
    submit::{
        execute::{Action, BookmarkNameOrPendingChangeId},
        plan::SubmissionPlan,
    },
};

const DEFAULT_SPEC_IMPACT_PATTERN: &str = r"(?im)^[\t ]*>?[\t ]*spec-impact:[\t ]*([^\r\n]+)$";

/// Validate the effective planned PR metadata before any push or fetch.
pub fn check_submission(
    jj: &Jujutsu,
    config: &Config,
    graph: &BookmarkGraph<'_>,
    plan: &SubmissionPlan,
) -> Result<()> {
    let Some(checks) = config.checks.as_ref() else {
        return Ok(());
    };
    let checks = CompiledChecks::new(checks)?;
    let metadata = planned_metadata(plan, config);

    for component in graph.components() {
        for bookmark in component.all_bookmarks() {
            let branch = if bookmark.is_pending() {
                jj.generated_push_bookmark(bookmark.bookmark.change_id())?
            } else {
                bookmark.name().to_owned()
            };
            checks.check_branch(&branch)?;
            let key = BookmarkNameOrPendingChangeId::new_from_pointer(bookmark).to_string();
            let (title, body, target_branch) = metadata.get(&key).ok_or_else(|| {
                ConfigSnafu {
                    message: format!("no planned PR metadata for {branch}"),
                }
                .build()
            })?;
            let title = if bookmark.is_pending() {
                title.replace(bookmark.name(), &branch)
            } else {
                title.clone()
            };
            checks.check_title(&title)?;

            if let Some(spec_impact) = &checks.spec_impact {
                if spec_impact.is_exempt(&branch) {
                    continue;
                }
                let changed_spec = if spec_impact.config.paths.is_empty() {
                    false
                } else {
                    let from = match bookmark.parents.first() {
                        Some(crate::bookmark::BookmarkRef::Bookmark(parent))
                            if parent.name() == target_branch =>
                        {
                            parent.name_for_jj()
                        }
                        _ => {
                            let base = crate::jj::revset_string_literal(target_branch);
                            let remote = crate::jj::revset_string_literal(&config.remote_name);
                            format!(
                                "coalesce(remote_bookmarks(exact:{base}, exact:{remote}), bookmarks(exact:{base}), trunk())"
                            )
                        }
                    };
                    jj.exec([
                        "diff",
                        "--from",
                        &format!("heads(::({from}) & ::({}))", bookmark.name_for_jj()),
                        "--to",
                        &bookmark.name_for_jj(),
                        "--name-only",
                    ])?
                    .stdout
                    .lines()
                    .any(|path| {
                        spec_impact
                            .config
                            .paths
                            .iter()
                            .any(|prefix| path.starts_with(prefix))
                    })
                };
                spec_impact.check(&branch, body, changed_spec)?;
            }
        }
    }

    Ok(())
}

fn planned_metadata(
    plan: &SubmissionPlan,
    config: &Config,
) -> HashMap<String, (String, String, String)> {
    let mut metadata = plan
        .existing_mrs
        .iter()
        .map(|(branch, mr)| {
            (
                branch.clone(),
                (
                    mr.title().to_owned(),
                    mr.description().to_owned(),
                    mr.target_branch().to_owned(),
                ),
            )
        })
        .collect::<HashMap<_, _>>();
    for action in plan.actions.iter().flatten() {
        match action {
            Action::CreateMR(action) => {
                metadata.insert(
                    action.bookmark.to_string(),
                    (
                        action.title.clone(),
                        action.description.clone(),
                        action.target_branch.clone(),
                    ),
                );
            }
            Action::UpdateMRTitleDescription(action) => {
                if let Some((title, body, _)) = metadata.get_mut(&action.bookmark.to_string()) {
                    if let Some(new_title) = &action.title {
                        title.clone_from(new_title);
                    }
                    if let Some(new_body) = &action.description {
                        body.clone_from(new_body);
                    }
                    *body = crate::description::insert_stack_into_description(
                        "",
                        body,
                        config.description.placement,
                    );
                }
            }
            Action::UpdateMRBase(action) => {
                if let Some((_, _, base)) = metadata.get_mut(&action.bookmark) {
                    base.clone_from(&action.new_target_branch);
                }
            }
            _ => {}
        }
    }
    metadata
}

struct CompiledChecks {
    config: SubmitChecksConfig,
    branch: Option<Regex>,
    title: Option<Regex>,
    spec_impact: Option<CompiledSpecImpact>,
}

impl CompiledChecks {
    fn new(config: &SubmitChecksConfig) -> Result<Self> {
        Ok(Self {
            config: config.clone(),
            branch: config
                .branch_pattern
                .as_deref()
                .map(|pattern| compile_pattern("branch-name", pattern))
                .transpose()?,
            title: config
                .title_pattern
                .as_deref()
                .map(|pattern| compile_pattern("pr-title-issue-ref", pattern))
                .transpose()?,
            spec_impact: config
                .spec_impact
                .as_ref()
                .map(CompiledSpecImpact::new)
                .transpose()?,
        })
    }

    fn check_branch(&self, branch: &str) -> Result<()> {
        let codename = branch.split('/').next().unwrap_or_default();
        if self
            .config
            .reserved_codenames
            .iter()
            .any(|reserved| reserved == codename)
            || self
                .branch
                .as_ref()
                .is_some_and(|pattern| !pattern.is_match(branch))
        {
            whatever!(
                "submit check failed: branch-name value '{branch}'; expected a branch matching '{}' with a non-reserved codename; use a conforming bookmark name",
                self.config.branch_pattern.as_deref().unwrap_or_default()
            );
        }
        Ok(())
    }

    fn check_title(&self, title: &str) -> Result<()> {
        let Some(pattern) = &self.title else {
            return Ok(());
        };
        let title = title.lines().next().unwrap_or_default();
        if !pattern.is_match(title) {
            whatever!(
                "submit check failed: pr-title-issue-ref value '{title}'; expected a title matching '{}'; add the required issue reference to the commit subject",
                self.config.title_pattern.as_deref().unwrap_or_default()
            );
        }
        Ok(())
    }
}

struct CompiledSpecImpact {
    config: SpecImpactCheckConfig,
    body_line: Regex,
}

impl CompiledSpecImpact {
    fn new(config: &SpecImpactCheckConfig) -> Result<Self> {
        let body_line = compile_pattern(
            "spec-impact bodyLine",
            config
                .body_line
                .as_deref()
                .unwrap_or(DEFAULT_SPEC_IMPACT_PATTERN),
        )?;
        Ok(Self {
            config: config.clone(),
            body_line,
        })
    }

    fn is_exempt(&self, branch: &str) -> bool {
        self.config
            .exempt_branch_prefixes
            .iter()
            .any(|prefix| branch.starts_with(prefix))
    }

    fn check(&self, branch: &str, body: &str, changed_spec: bool) -> Result<()> {
        if changed_spec || self.has_declaration(body) {
            return Ok(());
        }
        whatever!(
            "submit check failed: spec-impact value '{branch}'; no configured spec path changed and the PR body has no non-empty Spec-impact line; update a spec or add a Spec-impact declaration"
        );
    }

    fn has_declaration(&self, body: &str) -> bool {
        body.lines().any(|line| {
            self.body_line
                .captures(line)
                .and_then(|captures| captures.get(1))
                .is_some_and(|value| !value.as_str().trim().is_empty())
        })
    }
}

fn compile_pattern(rule: &str, pattern: &str) -> Result<Regex> {
    Regex::new(pattern).map_err(|error| {
        ConfigSnafu {
            message: format!("jj-vine.checks {rule} pattern is invalid: {error}"),
        }
        .build()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_checks_are_inert() {
        let temp = tempfile::TempDir::new().expect("temporary directory");
        let jj = Jujutsu::new(temp.path()).expect("jj executable");
        let config = Config::builder()
            .forge(crate::config::ForgeType::GitHub)
            .build();
        let graph = BookmarkGraph::from_lookups(
            std::collections::BTreeMap::default(),
            &std::collections::BTreeMap::default(),
        );
        let plan = SubmissionPlan {
            actions: Vec::new(),
            existing_mrs: HashMap::new(),
        };
        assert!(check_submission(&jj, &config, &graph, &plan).is_ok());
    }

    #[test]
    fn branch_pattern_and_reserved_codenames_match_gate_rules() {
        let checks = SubmitChecksConfig {
            branch_pattern: Some(r"^[a-z][a-z0-9-]*/rig-[0-9]+-[a-z0-9][a-z0-9-]*$".to_owned()),
            reserved_codenames: vec![
                "mintaka".to_owned(),
                "seal".to_owned(),
                "renovate".to_owned(),
                "dependabot".to_owned(),
                "trunk-merge".to_owned(),
                "graphite-base".to_owned(),
            ],
            ..SubmitChecksConfig::default()
        };
        let compiled = CompiledChecks::new(&checks).expect("configured gates compile");
        assert!(compiled.check_branch("hudson/rig-42-fix-leak").is_ok());
        assert!(compiled.check_branch("mintaka/rig-42-fix-leak").is_err());
        assert!(compiled.check_branch("hudson-rig-42-fix-leak").is_err());
    }

    #[test]
    fn title_pattern_matches_orion_issue_reference_rule() {
        let checks = SubmitChecksConfig {
            title_pattern: Some(r"(?i)\bRIG-[0-9]+".to_owned()),
            ..SubmitChecksConfig::default()
        };
        let compiled = CompiledChecks::new(&checks).expect("configured title gate compiles");
        assert!(
            compiled
                .check_title("fix: report push output (rig-42)")
                .is_ok()
        );
        assert!(compiled.check_title("fix: report push output").is_err());
        assert!(
            compiled
                .check_title("fix: missing reference\n\nRefs RIG-42")
                .is_err()
        );
    }

    #[test]
    fn spec_impact_matches_orion_line_and_exemption_rules() {
        let spec = CompiledSpecImpact::new(&SpecImpactCheckConfig {
            paths: vec![
                "docs/specs/".to_owned(),
                "personal/matt/docs/specs/".to_owned(),
            ],
            body_line: Some(DEFAULT_SPEC_IMPACT_PATTERN.to_owned()),
            exempt_branch_prefixes: vec!["renovate/".to_owned(), "trunk-merge/".to_owned()],
        })
        .expect("configured spec gate compiles");
        assert!(
            spec.check("hudson/rig-42-fix-leak", "Spec-impact: none\n", false)
                .is_ok()
        );
        assert!(
            spec.check(
                "hudson/rig-42-fix-leak",
                "> Spec-impact: update docs\n",
                false
            )
            .is_ok()
        );
        assert!(
            spec.check("hudson/rig-42-fix-leak", "Spec-impact:\n", false)
                .is_err()
        );
        assert!(
            spec.check("feature", "Spec-impact:\n\nUnrelated notes", false)
                .is_err()
        );
        assert!(
            spec.check(
                "hudson/rig-42-fix-leak",
                "summary mentions Spec-impact: none",
                false
            )
            .is_err()
        );
        assert!(spec.check("hudson/rig-42-fix-leak", "", true).is_ok());
        assert!(spec.is_exempt("renovate/dependency"));
        assert!(spec.is_exempt("trunk-merge/try"));
        assert!(!spec.is_exempt("hudson/rig-42-fix-leak"));
    }
}
