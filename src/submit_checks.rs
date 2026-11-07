use std::{collections::HashSet, path::Path};

use regex::Regex;
use snafu::whatever;

use crate::{
    bookmark::BookmarkGraph,
    config::{Config, SpecImpactCheckConfig, SubmitChecksConfig},
    error::{ConfigSnafu, Result},
    jj::{Change, Jujutsu},
    title::get_mr_title,
};

const DEFAULT_SPEC_IMPACT_PATTERN: &str = r"(?im)^\s*>?\s*spec-impact:\s*(\S.*)$";

/// Validate configured metadata gates before submit performs forge calls or
/// pushes.
pub fn check_submission(
    jj: &Jujutsu,
    config: &Config,
    graph: &BookmarkGraph<'_>,
    repository_root: &Path,
) -> Result<()> {
    let Some(checks) = config.checks.as_ref() else {
        return Ok(());
    };
    let checks = CompiledChecks::new(checks)?;
    let root_base_bookmark = config.default_base_branch.as_deref().map(|branch| {
        let branch = crate::jj::revset_string_literal(branch);
        let remote = crate::jj::revset_string_literal(&config.remote_name);
        format!("coalesce(remote_bookmarks(exact:{branch}, exact:{remote}), trunk())")
    });

    for component in graph.components() {
        for bookmark in component.all_bookmarks() {
            let branch = bookmark.name();
            checks.check_branch(branch)?;
            let revisions = bookmark.revisions(jj, root_base_bookmark.as_deref())?;
            let title = get_mr_title(jj, config, bookmark, component, &revisions)?;
            checks.check_title(&title)?;

            if let Some(spec_impact) = &checks.spec_impact {
                if spec_impact.is_exempt(branch) {
                    continue;
                }
                let generated_body = crate::description::generate_description(
                    &config.description,
                    &revisions,
                    repository_root,
                );
                let body = if generated_body.trim().is_empty() {
                    revisions
                        .first()
                        .map_or("", Change::description_not_first_line)
                } else {
                    generated_body.as_str()
                };
                let changed_spec = if spec_impact.config.paths.is_empty() {
                    false
                } else {
                    changed_paths(jj, &revisions)?.iter().any(|path| {
                        spec_impact
                            .config
                            .paths
                            .iter()
                            .any(|prefix| path.starts_with(prefix))
                    })
                };
                spec_impact.check(branch, body, changed_spec)?;
            }
        }
    }

    Ok(())
}

fn changed_paths(jj: &Jujutsu, revisions: &[Change]) -> Result<HashSet<String>> {
    let mut paths = HashSet::new();
    for revision in revisions {
        let output = jj.exec([
            "diff",
            "--revisions",
            revision.change_id.as_str(),
            "--name-only",
        ])?;
        paths.extend(
            output
                .stdout
                .lines()
                .filter(|path| !path.is_empty())
                .map(str::to_owned),
        );
    }
    Ok(paths)
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
        let Some(pattern) = &self.branch else {
            return Ok(());
        };
        let codename = branch.split('/').next().unwrap_or_default();
        if self
            .config
            .reserved_codenames
            .iter()
            .any(|reserved| reserved == codename)
            || !pattern.is_match(branch)
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
        self.body_line
            .captures(body)
            .and_then(|captures| captures.get(1))
            .is_some_and(|value| !value.as_str().trim().is_empty())
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
        assert!(check_submission(&jj, &config, &graph, Path::new(".")).is_ok());
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
