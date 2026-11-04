//! `nexus classification [show | levels | set]`: the project's
//! confidentiality level and what it drives (gateway classification, lane
//! delegates, blocked external services), via `GET`/`PATCH
//! /api/mcp/projects/{id}/classification`. Levels, labels, the gateway
//! mapping and the blocked services come from the hub and are never cached
//! locally; the CLI checks the downgrade rule only for its prompts, the hub
//! enforces it.

use console::style;
use nexus_core::api::{
    ClassificationGateAction, ClassificationLevel, ClassificationPatchOutcome,
    ClassificationPatchResponse, ClassificationResponse, NexusClient,
};
use nexus_core::config;

use super::claude_render::is_restricted_classification;

/// Fetch the classification, turning a 404 into a clear "hub too old" hint.
async fn fetch(client: &NexusClient, project_id: &str) -> anyhow::Result<ClassificationResponse> {
    match client.get_project_classification(project_id).await {
        Err(nexus_core::Error::NotFound(_)) => anyhow::bail!(
            "this Nexus backend does not offer the project classification yet \
             (GET /api/mcp/projects/{{id}}/classification); see it in the Nexus dashboard"
        ),
        other => Ok(other?),
    }
}

/// The classification for a status or pre-launch line; `None` when the
/// hub does not offer it or cannot be reached.
pub(crate) async fn try_fetch(
    client: &NexusClient,
    project_id: &str,
) -> Option<ClassificationResponse> {
    client.get_project_classification(project_id).await.ok()
}

/// `rendered` or `suppressed`: what happens to lane delegates under a
/// gateway classification.
pub(crate) fn delegates_for(gateway: Option<&str>) -> &'static str {
    if is_restricted_classification(gateway) {
        "suppressed"
    } else {
        "rendered"
    }
}

/// `<effective> (<gateway class>, from <project|customer>)`.
pub(crate) fn summary_line(c: &ClassificationResponse) -> String {
    format!(
        "{} ({}, from {})",
        c.effective_level,
        c.gateway_classification,
        c.effective_source.as_deref().unwrap_or("project")
    )
}

/// A level with the hub's label, e.g. `vs-nfd (VS-NfD)`.
fn level_name(levels: &[ClassificationLevel], value: &str) -> String {
    match levels
        .iter()
        .find(|l| l.value == value)
        .and_then(|l| l.label.as_deref())
    {
        Some(label) if label != value => format!("{value} ({label})"),
        _ => value.to_string(),
    }
}

fn opt_level(levels: &[ClassificationLevel], value: Option<&str>) -> String {
    value.map_or_else(|| "(none)".to_string(), |v| level_name(levels, v))
}

/// Gate action ids with the hub's labels, e.g. `Embeddings (embedding_generation)`.
fn action_names(ids: &[String], actions: &[ClassificationGateAction]) -> String {
    if ids.is_empty() {
        return "none".to_string();
    }
    ids.iter()
        .map(|id| {
            match actions
                .iter()
                .find(|a| &a.id == id)
                .and_then(|a| a.label.as_deref())
            {
                Some(label) if label != id => format!("{label} ({id})"),
                _ => id.clone(),
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Delegates as the hub reports them, else derived from the gateway class.
fn delegates_of(c: &ClassificationResponse) -> String {
    c.delegates
        .clone()
        .unwrap_or_else(|| delegates_for(Some(&c.gateway_classification)).to_string())
}

/// Where the gateway may route this project.
fn routing(c: &ClassificationResponse) -> String {
    match &c.allowed_profiles {
        Some(p) if !p.is_empty() => p.join(", "),
        _ if is_restricted_classification(Some(&c.gateway_classification)) => {
            "local model profiles only".to_string()
        }
        _ => "as the route policies allow".to_string(),
    }
}

/// The lines of `nexus classification show`.
fn show_lines(c: &ClassificationResponse) -> Vec<String> {
    let levels = &c.levels;
    let customer = match &c.customer {
        Some(cu) if cu.level.is_some() || cu.name.is_some() => format!(
            "{}{}",
            opt_level(levels, cu.level.as_deref()),
            cu.name
                .as_deref()
                .map(|n| format!(", customer {n}"))
                .unwrap_or_default()
        ),
        _ => "(no customer)".to_string(),
    };
    let mut gateway = c.gateway_classification.clone();
    let details: Vec<String> = [
        c.severity.as_deref().map(|s| format!("severity {s}")),
        c.mapping_version.map(|v| format!("mapping v{v}")),
    ]
    .into_iter()
    .flatten()
    .collect();
    if !details.is_empty() {
        gateway = format!("{gateway} ({})", details.join(", "));
    }
    let delegates = delegates_of(c);
    let delegates = if delegates == "suppressed" {
        "suppressed (start lane actors with nexus actors run <slug> or nexus run --actor-panes)"
            .to_string()
    } else {
        delegates.to_string()
    };
    vec![
        format!(
            "Project level:    {}",
            opt_level(levels, c.project_level.as_deref())
        ),
        format!("Customer level:   {customer}"),
        format!(
            "Effective level:  {} (from {})",
            level_name(levels, &c.effective_level),
            c.effective_source.as_deref().unwrap_or("project")
        ),
        format!("Gateway class:    {gateway}"),
        String::new(),
        "Consequences".to_string(),
        format!("  Lane delegates:     {delegates}"),
        format!("  Gateway routing:    {}", routing(c)),
        format!(
            "  Blocked services:   {}",
            action_names(&c.blocked_actions, &c.actions)
        ),
    ]
}

fn show_json(c: &ClassificationResponse) -> serde_json::Value {
    let mut v = serde_json::to_value(c).unwrap_or_default();
    v["consequences"] = serde_json::json!({
        "delegates": delegates_of(c),
        "gateway_routing": routing(c),
        "blocked_actions": c.blocked_actions,
    });
    v
}

/// `nexus classification [show]`.
pub async fn show(api_url: &str, json: bool) -> anyhow::Result<()> {
    let (project_id, client) = super::env_cmd::connect(api_url)?;
    let c = fetch(&client, &project_id).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&show_json(&c))?);
        return Ok(());
    }
    println!("{}", style("Classification").bold());
    println!();
    for line in show_lines(&c) {
        println!("  {line}");
    }
    Ok(())
}

/// The table of `nexus classification levels`, columns sized to fit.
fn level_rows(c: &ClassificationResponse) -> Vec<String> {
    let cell = |v: &Option<String>| v.clone().unwrap_or_default();
    let rows: Vec<[String; 4]> = c
        .levels
        .iter()
        .map(|l| {
            [
                l.value.clone(),
                cell(&l.label),
                cell(&l.group),
                format!(
                    "{}{}",
                    cell(&l.gateway_classification),
                    if l.value == c.effective_level {
                        "  (effective)"
                    } else {
                        ""
                    }
                ),
            ]
        })
        .collect();
    let header = [
        "LEVEL".to_string(),
        "LABEL".to_string(),
        "GROUP".to_string(),
        "GATEWAY CLASS".to_string(),
    ];
    let width = |i: usize| {
        rows.iter()
            .chain(std::iter::once(&header))
            .map(|r| r[i].chars().count())
            .max()
            .unwrap_or(0)
    };
    let (w0, w1, w2) = (width(0), width(1), width(2));
    let pad = |s: &str, w: usize| format!("{s}{}", " ".repeat(w.saturating_sub(s.chars().count())));
    std::iter::once(&header)
        .chain(rows.iter())
        .map(|r| {
            format!(
                "{}  {}  {}  {}",
                pad(&r[0], w0),
                pad(&r[1], w1),
                pad(&r[2], w2),
                r[3]
            )
            .trim_end()
            .to_string()
        })
        .collect()
}

/// `nexus classification levels`.
pub async fn levels(api_url: &str, json: bool) -> anyhow::Result<()> {
    let (project_id, client) = super::env_cmd::connect(api_url)?;
    let c = fetch(&client, &project_id).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&c.levels)?);
        return Ok(());
    }
    for line in level_rows(&c) {
        println!("  {line}");
    }
    Ok(())
}

fn change(before: Option<&str>, after: Option<&str>) -> String {
    let b = before.unwrap_or("(none)");
    let a = after.unwrap_or("(none)");
    if b == a {
        format!("{b} (unchanged)")
    } else {
        format!("{b} -> {a}")
    }
}

/// Whether the effective level stays as it is because the customer level
/// dominates.
fn customer_dominates(plan: &ClassificationPatchResponse) -> bool {
    plan.warnings.iter().any(|w| w == "customer_dominates")
        || (plan.before.project_level != plan.after.project_level
            && plan.before.effective_level == plan.after.effective_level)
}

/// The impact plan of `nexus classification set`.
fn plan_lines(
    plan: &ClassificationPatchResponse,
    customer_level: Option<&str>,
    actions: &[ClassificationGateAction],
) -> Vec<String> {
    let (b, a) = (&plan.before, &plan.after);
    let delegates_after = plan
        .impact
        .delegates
        .clone()
        .unwrap_or_else(|| delegates_for(a.gateway_classification.as_deref()).to_string());
    let mut lines = vec![
        format!(
            "Project level:    {}",
            change(b.project_level.as_deref(), a.project_level.as_deref())
        ),
        format!(
            "Effective level:  {}",
            change(b.effective_level.as_deref(), a.effective_level.as_deref())
        ),
        format!(
            "Gateway class:    {}{}",
            change(
                b.gateway_classification.as_deref(),
                a.gateway_classification.as_deref()
            ),
            if plan.impact.downgrade {
                " (downgrade)"
            } else {
                ""
            }
        ),
        format!(
            "Lane delegates:   {}",
            change(
                Some(delegates_for(b.gateway_classification.as_deref())),
                Some(&delegates_after)
            )
        ),
        format!(
            "Newly allowed:    {}",
            action_names(&plan.impact.newly_allowed_actions, actions)
        ),
        format!(
            "Newly blocked:    {}",
            action_names(&plan.impact.newly_blocked_actions, actions)
        ),
    ];
    if customer_dominates(plan) {
        lines.push(String::new());
        lines.push(format!(
            "The customer level{} dominates: the effective level does not change. \
             The customer level is the real lever (customer admins, Nexus dashboard).",
            customer_level
                .map(|l| format!(" ({l})"))
                .unwrap_or_default()
        ));
    }
    lines
}

/// What `set` does after the plan.
#[derive(Debug, PartialEq, Eq)]
enum Decision {
    /// Nothing to change; the reason is printed.
    NoOp(String),
    DryRun,
    /// A gateway-class downgrade without `--reason`.
    NeedsReason,
    /// Downgrade: the user types the project slug.
    ConfirmDowngrade,
    Confirm,
}

fn decide(
    plan: &ClassificationPatchResponse,
    reason: Option<&str>,
    force: bool,
    dry_run: bool,
) -> Decision {
    if plan.impact.project_level_changed == Some(false)
        || plan.before.project_level == plan.after.project_level
    {
        return Decision::NoOp(format!(
            "The project level is already {}.",
            plan.after.project_level.as_deref().unwrap_or("(none)")
        ));
    }
    if dry_run {
        return Decision::DryRun;
    }
    if customer_dominates(plan) && !force {
        return Decision::NoOp(
            "Not changed: it would have no effect (pass --force to store the project level anyway)."
                .into(),
        );
    }
    let has_reason = reason.is_some_and(|r| !r.trim().is_empty());
    match (plan.impact.downgrade, has_reason) {
        (true, false) => Decision::NeedsReason,
        (true, true) => Decision::ConfirmDowngrade,
        (false, _) => Decision::Confirm,
    }
}

/// What follows a `set`: pull, print the pull hint, or nothing.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AfterSet {
    Pull,
    Hint,
    Nothing,
}

pub(crate) fn after_set(changed: bool, pull: bool) -> AfterSet {
    match (changed, pull) {
        (true, true) => AfterSet::Pull,
        (true, false) => AfterSet::Hint,
        (false, _) => AfterSet::Nothing,
    }
}

fn interactive() -> bool {
    console::Term::stdout().is_term() && console::user_attended()
}

fn prompt(question: &str) -> anyhow::Result<String> {
    use std::io::Write as _;
    print!("   {} {} ", style("?").bold().cyan(), question);
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().to_string())
}

fn unknown_level(level: &str, levels: &[ClassificationLevel]) -> anyhow::Error {
    anyhow::anyhow!(
        "unknown classification level {level}; valid: {} (nexus classification levels)",
        levels
            .iter()
            .map(|l| l.value.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// Refuse a change up front when the hub says the caller cannot write; a
/// dry run is still sent (the hub answers 403 if it needs rights too).
fn write_check(can_write: Option<bool>, dry_run: bool) -> anyhow::Result<()> {
    if can_write == Some(false) && !dry_run {
        anyhow::bail!(
            "you cannot change this project's classification (needs project admin rights, project.manage)"
        );
    }
    Ok(())
}

/// Map the 4xx outcomes of a PATCH to errors; `Done` passes through.
fn expect_done(
    outcome: ClassificationPatchOutcome,
) -> anyhow::Result<Result<ClassificationPatchResponse, String>> {
    match outcome {
        ClassificationPatchOutcome::Done(resp) => Ok(Ok(*resp)),
        ClassificationPatchOutcome::Conflict(error) => Ok(Err(error)),
        ClassificationPatchOutcome::Invalid(error) => {
            anyhow::bail!("classification rejected by the backend: {error}")
        }
        ClassificationPatchOutcome::Forbidden(error) => {
            anyhow::bail!("{error} (changing the classification needs project admin rights)")
        }
        ClassificationPatchOutcome::ReasonRequired(error) => {
            anyhow::bail!("{error}: a gateway-class downgrade needs --reason \"<text>\"")
        }
    }
}

/// Print the state after a 409 and stop.
async fn conflict(client: &NexusClient, project_id: &str, error: &str) -> anyhow::Result<bool> {
    println!(
        "   {} {error}: the classification changed since it was read. Current state:",
        style("!").bold().yellow()
    );
    if let Ok(fresh) = fetch(client, project_id).await {
        for line in show_lines(&fresh) {
            println!("      {line}");
        }
    }
    anyhow::bail!("not applied; re-run nexus classification set")
}

/// `nexus classification set <level>`. Returns whether the level changed.
#[allow(clippy::too_many_arguments)]
pub async fn set(
    api_url: &str,
    level: &str,
    reason: Option<&str>,
    dry_run: bool,
    force: bool,
    assume_yes: bool,
    json: bool,
) -> anyhow::Result<bool> {
    let (project_id, client) = super::env_cmd::connect(api_url)?;
    let current = fetch(&client, &project_id).await?;
    write_check(current.can_write, dry_run)?;
    if !current.levels.is_empty() && !current.levels.iter().any(|l| l.value == level) {
        return Err(unknown_level(level, &current.levels));
    }

    // Always plan first; the hub computes the impact.
    let plan = match expect_done(
        client
            .patch_project_classification(&project_id, level, reason, true, &current.revision)
            .await?,
    )? {
        Ok(plan) => plan,
        Err(error) => return conflict(&client, &project_id, &error).await,
    };
    let customer_level = current
        .customer
        .as_ref()
        .and_then(|c| c.level.as_deref())
        .map(|l| level_name(&current.levels, l));
    if json && dry_run {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(false);
    }
    if !json {
        println!("{}", style("Classification change (plan)").bold());
        println!();
        for line in plan_lines(&plan, customer_level.as_deref(), &current.actions) {
            println!("  {line}");
        }
        println!();
    }

    match decide(&plan, reason, force, dry_run) {
        Decision::NoOp(why) => {
            println!("   {} {why}", style("i").bold().blue());
            return Ok(false);
        }
        Decision::DryRun => {
            println!("   {} Dry run: nothing changed.", style("i").bold().blue());
            return Ok(false);
        }
        Decision::NeedsReason => anyhow::bail!(
            "this lowers the gateway class ({} -> {}); pass --reason \"<text>\" (recorded in the audit log)",
            plan.before.gateway_classification.as_deref().unwrap_or("?"),
            plan.after.gateway_classification.as_deref().unwrap_or("?")
        ),
        Decision::ConfirmDowngrade => {
            if !interactive() {
                anyhow::bail!(
                    "lowering the gateway class needs an interactive confirmation (-y does not apply)"
                );
            }
            let slug = current
                .project_slug
                .clone()
                .or_else(|| {
                    config::load_linked_project(None)
                        .ok()
                        .flatten()
                        .map(|p| p.name)
                })
                .unwrap_or_else(|| project_id.clone());
            if prompt(&format!("Type the project slug ({slug}) to confirm:"))? != slug {
                anyhow::bail!("not confirmed; nothing changed");
            }
        }
        Decision::Confirm => {
            if !assume_yes {
                if !interactive() {
                    anyhow::bail!("not a terminal: pass -y to apply the change");
                }
                let answer = prompt("Apply this change? [y/N]")?;
                if !matches!(answer.to_lowercase().as_str(), "y" | "yes") {
                    anyhow::bail!("not applied");
                }
            }
        }
    }

    let revision = plan.revision.as_deref().unwrap_or(&current.revision);
    let applied = match expect_done(
        client
            .patch_project_classification(&project_id, level, reason, false, revision)
            .await?,
    )? {
        Ok(resp) => resp,
        Err(error) => return conflict(&client, &project_id, &error).await,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&applied)?);
    } else if applied.applied {
        println!(
            "   {} Classification: {}, gateway class {}",
            style("✓").green(),
            change(
                applied.before.effective_level.as_deref(),
                applied.after.effective_level.as_deref()
            ),
            change(
                applied.before.gateway_classification.as_deref(),
                applied.after.gateway_classification.as_deref()
            )
        );
    }
    Ok(applied.applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use nexus_core::api::{ClassificationImpact, ClassificationState};

    fn state(project: &str, effective: &str, gateway: &str) -> ClassificationState {
        ClassificationState {
            project_level: Some(project.into()),
            effective_level: Some(effective.into()),
            gateway_classification: Some(gateway.into()),
            ..Default::default()
        }
    }

    fn plan(
        before: ClassificationState,
        after: ClassificationState,
        downgrade: bool,
    ) -> ClassificationPatchResponse {
        ClassificationPatchResponse {
            applied: false,
            before,
            after,
            impact: ClassificationImpact {
                gateway_classification_changed: downgrade,
                downgrade,
                ..Default::default()
            },
            warnings: vec![],
            revision: Some("rev-2".into()),
        }
    }

    fn downgrade() -> ClassificationPatchResponse {
        let mut p = plan(
            state(
                "strictly-confidential",
                "strictly-confidential",
                "restricted",
            ),
            state("confidential", "confidential", "confidential"),
            true,
        );
        p.impact.delegates = Some("rendered".into());
        p.impact.newly_allowed_actions = vec!["embedding_generation".into()];
        p
    }

    #[test]
    fn test_plan_downgrade_shows_old_and_new() {
        let lines = plan_lines(&downgrade(), None, &[]).join("\n");
        assert!(lines.contains("strictly-confidential -> confidential"));
        assert!(lines.contains("restricted -> confidential (downgrade)"));
        assert!(lines.contains("suppressed -> rendered"));
        assert!(lines.contains("Newly allowed:    embedding_generation"));
        assert!(lines.contains("Newly blocked:    none"));
        assert!(!lines.contains("dominates"));
    }

    #[test]
    fn test_plan_upgrade() {
        let p = plan(
            state("internal", "internal", "internal"),
            state("geheim", "geheim", "restricted"),
            false,
        );
        let lines = plan_lines(&p, None, &[]).join("\n");
        assert!(lines.contains("internal -> restricted"));
        assert!(!lines.contains("(downgrade)"));
        assert!(lines.contains("rendered -> suppressed"));
        assert_eq!(decide(&p, None, false, false), Decision::Confirm);
    }

    #[test]
    fn test_customer_dominates_is_a_noop_unless_forced() {
        let mut p = plan(
            state("internal", "geheim", "restricted"),
            state("public", "geheim", "restricted"),
            false,
        );
        p.warnings = vec!["customer_dominates".into()];
        let lines = plan_lines(&p, Some("geheim"), &[]).join("\n");
        assert!(lines.contains("customer level (geheim) dominates"));
        assert!(lines.contains("geheim (unchanged)"));
        assert!(matches!(decide(&p, None, false, false), Decision::NoOp(_)));
        assert_eq!(decide(&p, None, true, false), Decision::Confirm);
        // Detected from the states even without the warning.
        p.warnings.clear();
        assert!(customer_dominates(&p));
    }

    #[test]
    fn test_same_level_and_dry_run() {
        let p = plan(
            state("internal", "internal", "internal"),
            state("internal", "internal", "internal"),
            false,
        );
        assert!(matches!(decide(&p, None, false, true), Decision::NoOp(_)));
        assert_eq!(decide(&downgrade(), None, false, true), Decision::DryRun);
    }

    #[test]
    fn test_downgrade_needs_a_reason() {
        let p = downgrade();
        assert_eq!(decide(&p, None, false, false), Decision::NeedsReason);
        assert_eq!(decide(&p, Some("  "), false, false), Decision::NeedsReason);
        assert_eq!(
            decide(&p, Some("lane test"), false, false),
            Decision::ConfirmDowngrade
        );
        // -y never skips the downgrade confirmation: decide ignores it.
        assert_eq!(
            decide(&p, Some("lane test"), true, false),
            Decision::ConfirmDowngrade
        );
    }

    #[test]
    fn test_after_set() {
        assert_eq!(after_set(true, true), AfterSet::Pull);
        assert_eq!(after_set(true, false), AfterSet::Hint);
        assert_eq!(after_set(false, true), AfterSet::Nothing);
    }

    fn response() -> ClassificationResponse {
        serde_json::from_value(serde_json::json!({
            "project_slug": "beta",
            "project_level": "strictly-confidential",
            "customer": {"id": "c1", "name": "Acme", "level": "internal"},
            "effective_level": "strictly-confidential",
            "effective_source": "project",
            "gateway_classification": "restricted",
            "severity": "medium",
            "mapping_version": 1,
            "blocked_actions": ["textract_extraction", "embedding_generation"],
            "revision": "rev-1",
            "levels": [
                {"value": "internal", "label": "Internal", "group": "standard", "gateway_classification": "internal"},
                {"value": "strictly-confidential", "label": "Strictly confidential", "group": "standard", "gateway_classification": "restricted"}
            ]
        }))
        .unwrap()
    }

    #[test]
    fn test_show_lines_and_json() {
        let c = response();
        let lines = show_lines(&c).join("\n");
        assert!(lines.contains("Customer level:   internal (Internal), customer Acme"));
        assert!(lines.contains("strictly-confidential (Strictly confidential) (from project)"));
        assert!(lines.contains("restricted (severity medium, mapping v1)"));
        assert!(lines.contains("Lane delegates:     suppressed"));
        assert!(lines.contains("local model profiles only"));
        assert!(lines.contains("textract_extraction, embedding_generation"));
        assert_eq!(
            summary_line(&c),
            "strictly-confidential (restricted, from project)"
        );
        let v = show_json(&c);
        assert_eq!(v["consequences"]["delegates"], "suppressed");
        assert_eq!(v["gateway_classification"], "restricted");
        assert_eq!(v["customer"]["name"], "Acme");
    }

    #[test]
    fn test_live_contract_fields() {
        let mut c = response();
        c.actions = vec![ClassificationGateAction {
            id: "textract_extraction".into(),
            label: Some("Document extraction".into()),
        }];
        c.delegates = Some("rendered".into());
        let lines = show_lines(&c).join("\n");
        assert!(lines.contains("Document extraction (textract_extraction), embedding_generation"));
        // The hub's decision wins over the local derivation.
        assert!(lines.contains("Lane delegates:     rendered"));
        assert_eq!(show_json(&c)["consequences"]["delegates"], "rendered");

        let mut p = downgrade();
        p.impact.project_level_changed = Some(false);
        assert!(matches!(decide(&p, None, false, false), Decision::NoOp(_)));
        let lines = plan_lines(&downgrade(), None, &c.actions).join("\n");
        assert!(lines.contains("Newly allowed:    embedding_generation"));

        assert!(write_check(Some(false), false).is_err());
        assert!(write_check(Some(false), true).is_ok());
        assert!(write_check(None, false).is_ok());
    }

    #[test]
    fn test_level_rows_align_long_labels() {
        let mut c = response();
        c.levels[0].label = Some("VS-NfD (Nur für den Dienstgebrauch)".into());
        let rows = level_rows(&c);
        let col = |r: &str| r[..r.find("standard").unwrap()].chars().count();
        assert_eq!(col(&rows[1]), col(&rows[2]));
        assert!(rows[2].ends_with("restricted  (effective)"));
        assert!(rows[0].starts_with("LEVEL"));
    }

    #[test]
    fn test_unknown_level_lists_valid_ones() {
        let msg = unknown_level("secret", &response().levels).to_string();
        assert!(msg.contains("internal, strictly-confidential"));
    }

    #[test]
    fn test_conflict_and_reason_outcomes() {
        assert_eq!(
            expect_done(ClassificationPatchOutcome::Conflict("changed".into()))
                .unwrap()
                .unwrap_err(),
            "changed"
        );
        let err = expect_done(ClassificationPatchOutcome::ReasonRequired("reason".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("--reason"));
    }
}
