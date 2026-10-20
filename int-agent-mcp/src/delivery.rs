use std::{
    collections::{BTreeMap, BTreeSet},
    io::ErrorKind,
};

use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt;

use crate::{
    command,
    model::{
        CiEvidence, Config, MergePolicy, PullRequest, ReviewReport, ReviewVerdict, Task, TaskState,
    },
    runtime,
    store::Store,
};

const REPORT_LIMIT: u64 = 1024 * 1024;

/// Advance one durable task. Errors deliberately retain integration states: an
/// unavailable GitHub API is not evidence that an integration lock can be freed.
pub async fn advance(config: &Config, store: &Store, task: &mut Task) -> Result<()> {
    let result = advance_inner(config, store, task).await;
    if let Err(error) = &result {
        let message = format!("{error:#}");
        if task.last_error.as_deref() != Some(message.as_str()) {
            task.last_error = Some(message.clone());
            store.save(task, "delivery_error", json!({"error": message}))?;
        }
    }
    result
}

async fn advance_inner(config: &Config, store: &Store, task: &mut Task) -> Result<()> {
    match task.state {
        TaskState::Archived
        | TaskState::Cancelled
        | TaskState::Failed
        | TaskState::Starting
        | TaskState::NeedsAttention => return Ok(()),
        TaskState::Completed => return finish_and_archive(config, store, task).await,
        TaskState::IntegrationBlocked => return flush_feedback(config, store, task).await,
        _ => {}
    }
    let Some(github) = &config.github else {
        ensure!(
            !task.state.integration(),
            "Task owns an integration gate but GitHub configuration is missing; restore configuration to reconcile its real merge/CI evidence"
        );
        return Ok(());
    };
    ensure!(
        !github.required_checks.is_empty(),
        "github.required_checks must not be empty"
    );
    ensure!(
        !github.post_merge_workflows.is_empty(),
        "github.post_merge_workflows must not be empty"
    );

    if task.state == TaskState::PostMergeCi {
        return post_merge_ci(config, store, task).await;
    }
    if task.state == TaskState::Merging {
        return reconcile_merge(config, store, task).await;
    }

    let Some(pr) = discover_pr(config, task).await? else {
        let message = format!(
            "Waiting for a same-repository pull request from {} into {} in {}. Push the Worker branch and open its PR; no CI or review has been assumed.",
            task.branch, config.default_branch, github.repository
        );
        return waiting(store, task, message);
    };
    validate_pr(config, task, &pr)?;
    if pr.merged {
        // A user can merge outside the controller. Never ignore that integration
        // merely because it bypassed our premerge gates.
        return record_merged(config, store, task, &pr, true).await;
    }
    if pr.state != "open" {
        return waiting(
            store,
            task,
            format!(
                "PR #{} is closed without a confirmed merge. Reopen it or resume the task with an open PR; no completion has been recorded.",
                pr.number
            ),
        );
    }
    adopt_pr(store, task, &pr)?;
    retire_stale_reviewer(config, store, task).await?;
    if pr.draft {
        return waiting(
            store,
            task,
            format!(
                "PR #{} is a draft; mark it ready before CI/review/merge delivery.",
                pr.number
            ),
        );
    }

    match pr_checks(config, &pr).await? {
        Gate::Waiting(message) => {
            if task.state != TaskState::PrCi {
                task.state = TaskState::PrCi;
                store.save(
                    task,
                    "pr_ci_waiting",
                    json!({"head_sha": pr.head.sha, "base_sha": pr.base.sha}),
                )?;
            }
            return waiting(store, task, message);
        }
        Gate::Failed(message) => {
            let state_changed = task.state != TaskState::ChangesRequested;
            task.state = TaskState::ChangesRequested;
            task.last_error = Some(message.clone());
            let key = format!("pr-ci:{}:{}", pr.base.sha, pr.head.sha);
            if state_changed || !event_seen(store, task, "pr_ci_failed", &key)? {
                store.save(task, "pr_ci_failed", json!({"key": key, "base_sha": pr.base.sha, "head_sha": pr.head.sha, "error": message}))?;
            }
            return feedback(config, store, task, &key, &format!(
                "Required PR CI failed for exact head {} (base {}).\n{}\nFix the failure in your task branch, push it, and leave CI enabled. Skipped/neutral/cancelled checks are not successful checks.",
                pr.head.sha, pr.base.sha, message
            )).await;
        }
        Gate::Passed(evidence) => {
            let key = format!("{}:{}", pr.base.sha, pr.head.sha);
            if !event_seen(store, task, "pr_ci_passed", &key)? {
                store.save(task, "pr_ci_passed", json!({"key": key, "base_sha": pr.base.sha, "head_sha": pr.head.sha, "evidence": evidence}))?;
            }
        }
    }

    if let Some(report) = task.review_report.as_ref() {
        if report.base_sha != pr.base.sha || report.head_sha != pr.head.sha {
            invalidate_review(
                store,
                task,
                &pr,
                "Persisted review does not match live PR base/head",
            )?;
            retire_stale_reviewer(config, store, task).await?;
            return Ok(());
        }
        let data = json!({"head_sha": report.head_sha, "base_sha": report.base_sha});
        match report.verdict {
            ReviewVerdict::ChangesRequested => {
                let key = format!("review:{}:{}", report.base_sha, report.head_sha);
                let message = format!(
                    "Independent Reviewer requested changes for head {} against base {}.\n{}\nFindings:\n{}\nVerification performed:\n{}\nAddress the findings and push a new head. The reviewer report is bound to these exact commits; it is not a GitHub approval.",
                    report.head_sha,
                    report.base_sha,
                    report.summary,
                    report.findings.join("\n"),
                    report.verification.join("\n")
                );
                if task.state != TaskState::ChangesRequested {
                    task.state = TaskState::ChangesRequested;
                    store.save(task, "review_changes_requested", data)?;
                }
                return feedback(config, store, task, &key, &message).await;
            }
            ReviewVerdict::Pass => {
                if task.state != TaskState::ReadyToMerge {
                    task.state = TaskState::ReadyToMerge;
                    task.last_error = None;
                    store.save(task, "ready_to_merge", data)?;
                }
                publish_review(config, store, task).await?;
                return begin_merge(config, store, task).await;
            }
        }
    }
    review(config, store, task, &pr).await
}

fn waiting(store: &Store, task: &mut Task, message: String) -> Result<()> {
    if task.last_error.as_deref() != Some(message.as_str()) {
        task.last_error = Some(message.clone());
        store.save(task, "delivery_waiting", json!({"reason": message}))?;
    }
    Ok(())
}

fn event_seen(store: &Store, task: &Task, kind: &str, key: &str) -> Result<bool> {
    Ok(store.events(&task.id)?.iter().any(|event| {
        event.kind == kind && event.data.get("key").and_then(Value::as_str) == Some(key)
    }))
}

async fn feedback(
    config: &Config,
    store: &Store,
    task: &mut Task,
    key: &str,
    text: &str,
) -> Result<()> {
    if event_seen(store, task, "worker_feedback_delivered", key)? {
        return Ok(());
    }
    if !event_seen(store, task, "worker_feedback_queued", key)? {
        store.save(
            task,
            "worker_feedback_queued",
            json!({"key": key, "message": text}),
        )?;
    }
    let Some(worker) = &task.worker else {
        return waiting(store, task, "Delivery feedback is durably queued, but this task has no Worker run. Resume its Worker to receive the CI/review findings.".into());
    };
    runtime::send_message(config, worker, text)
        .await
        .context("Delivering feedback to the Worker's durable inbox")?;
    store.save(task, "worker_feedback_delivered", json!({"key": key}))
}

async fn flush_feedback(config: &Config, store: &Store, task: &mut Task) -> Result<()> {
    let events = store.events(&task.id)?;
    for event in events
        .iter()
        .filter(|event| event.kind == "worker_feedback_queued")
    {
        let key = event
            .data
            .get("key")
            .and_then(Value::as_str)
            .context("Queued feedback has no durable key")?;
        let text = event
            .data
            .get("message")
            .and_then(Value::as_str)
            .context("Queued feedback has no durable message")?;
        feedback(config, store, task, key, text).await?;
    }
    Ok(())
}

fn repository(config: &Config) -> Result<&str> {
    Ok(&config
        .github
        .as_ref()
        .context("GitHub delivery is not configured")?
        .repository)
}

fn encode(value: &str) -> String {
    let mut result = String::with_capacity(value.len());
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            result.push(byte as char);
        } else {
            result.push('%');
            result.push(HEX[(byte >> 4) as usize] as char);
            result.push(HEX[(byte & 15) as usize] as char);
        }
    }
    result
}

async fn api(
    config: &Config,
    suffix: &str,
    params: &[(&str, &str)],
    paginate: bool,
) -> Result<Value> {
    let endpoint = format!("repos/{}/{}", repository(config)?, suffix);
    let mut args = vec![
        "api".to_owned(),
        "--method".into(),
        "GET".into(),
        endpoint,
        "-H".into(),
        "Accept: application/vnd.github+json".into(),
        "-H".into(),
        "X-GitHub-Api-Version: 2022-11-28".into(),
    ];
    if paginate {
        args.extend(["--paginate".into(), "--slurp".into()]);
    }
    for (key, value) in params {
        args.extend(["-f".into(), format!("{key}={value}")]);
    }
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = command::run(&config.gh, &args, &config.repository).await?;
    serde_json::from_str(&result).context("Parsing GitHub API JSON")
}

fn paged<T: serde::de::DeserializeOwned>(pages: Value, field: Option<&str>) -> Result<Vec<T>> {
    let Value::Array(pages) = pages else {
        bail!("GitHub paginated response is not a page array");
    };
    let mut result = Vec::new();
    for mut page in pages {
        let items = if let Some(field) = field {
            page.get_mut(field)
                .context("GitHub response is missing its result array")?
                .take()
        } else {
            page
        };
        let Value::Array(items) = items else {
            bail!("GitHub page is not a result array");
        };
        for item in items {
            result.push(serde_json::from_value(item).context("Parsing GitHub result item")?);
        }
    }
    Ok(result)
}

#[derive(Deserialize)]
struct ApiRepository {
    full_name: String,
}
#[derive(Deserialize)]
struct ApiRef {
    sha: String,
    #[serde(rename = "ref")]
    branch: String,
    repo: Option<ApiRepository>,
}
#[derive(Deserialize)]
struct ApiPull {
    number: u64,
    html_url: String,
    state: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    merged: bool,
    merge_commit_sha: Option<String>,
    head: ApiRef,
    base: ApiRef,
}

fn valid_sha(sha: &str) -> bool {
    sha.len() == 40 && sha.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn validate_pr(config: &Config, task: &Task, pr: &ApiPull) -> Result<()> {
    let repo = repository(config)?;
    ensure!(
        pr.head
            .repo
            .as_ref()
            .is_some_and(|head| head.full_name.eq_ignore_ascii_case(repo)),
        "PR #{} is not from the configured repository",
        pr.number
    );
    ensure!(
        pr.base
            .repo
            .as_ref()
            .is_some_and(|base| base.full_name.eq_ignore_ascii_case(repo)),
        "PR #{} targets a different repository",
        pr.number
    );
    ensure!(
        pr.head.branch == task.branch,
        "PR #{} no longer uses task branch {}",
        pr.number,
        task.branch
    );
    ensure!(
        pr.base.branch == config.default_branch,
        "PR #{} no longer targets {}",
        pr.number,
        config.default_branch
    );
    ensure!(
        valid_sha(&pr.head.sha) && valid_sha(&pr.base.sha),
        "PR #{} has invalid commit identities",
        pr.number
    );
    Ok(())
}

async fn get_pr(config: &Config, number: u64) -> Result<ApiPull> {
    serde_json::from_value(api(config, &format!("pulls/{number}"), &[], false).await?)
        .context("Parsing pull request")
}

async fn discover_pr(config: &Config, task: &Task) -> Result<Option<ApiPull>> {
    if let Some(pr) = &task.pr {
        return Ok(Some(get_pr(config, pr.number).await?));
    }
    let repo = repository(config)?;
    let owner = repo
        .split('/')
        .next()
        .context("GitHub repository has no owner")?;
    let head = format!("{owner}:{}", task.branch);
    let candidates: Vec<ApiPull> = paged(
        api(
            config,
            "pulls",
            &[
                ("state", "all"),
                ("head", &head),
                ("base", &config.default_branch),
                ("per_page", "100"),
            ],
            true,
        )
        .await?,
        None,
    )?;
    let mut matching = candidates
        .into_iter()
        .filter(|pr| {
            pr.head.branch == task.branch
                && pr.base.branch == config.default_branch
                && pr
                    .head
                    .repo
                    .as_ref()
                    .is_some_and(|head| head.full_name.eq_ignore_ascii_case(repo))
                && pr
                    .base
                    .repo
                    .as_ref()
                    .is_some_and(|base| base.full_name.eq_ignore_ascii_case(repo))
        })
        .collect::<Vec<_>>();
    matching.sort_unstable_by_key(|pr| std::cmp::Reverse(pr.number));
    ensure!(
        matching.iter().filter(|pr| pr.state == "open").count() <= 1,
        "Multiple open same-repository PRs match the task branch; resolve the ambiguity before delivery"
    );
    let selected = matching
        .iter()
        .find(|pr| pr.state == "open")
        .or_else(|| matching.first());
    match selected {
        Some(pr) => Ok(Some(get_pr(config, pr.number).await?)),
        None => Ok(None),
    }
}

fn snapshot(pr: &ApiPull) -> PullRequest {
    PullRequest {
        number: pr.number,
        url: pr.html_url.clone(),
        head_sha: pr.head.sha.clone(),
        base_sha: pr.base.sha.clone(),
    }
}

fn adopt_pr(store: &Store, task: &mut Task, pr: &ApiPull) -> Result<()> {
    let changed = task.pr.as_ref().is_some_and(|old| {
        old.number != pr.number || old.head_sha != pr.head.sha || old.base_sha != pr.base.sha
    });
    if changed {
        invalidate_review(
            store,
            task,
            pr,
            "Live PR head or base changed; CI, review and merge authorization invalidated",
        )
    } else if task.pr.is_none() {
        task.pr = Some(snapshot(pr));
        task.state = TaskState::PrCi;
        task.last_error = None;
        let data = json!({"pr": task.pr});
        store.save(task, "pr_discovered", data)
    } else {
        Ok(())
    }
}

fn invalidate_review(store: &Store, task: &mut Task, pr: &ApiPull, reason: &str) -> Result<()> {
    task.pr = Some(snapshot(pr));
    task.review_base = None;
    task.review_head = None;
    task.review_report = None;
    task.merge_authorized_head = None;
    task.state = TaskState::PrCi;
    task.last_error = Some(reason.into());
    store.save(
        task,
        "review_invalidated",
        json!({"reason": reason, "base_sha": pr.base.sha, "head_sha": pr.head.sha}),
    )
}

async fn retire_stale_reviewer(config: &Config, store: &Store, task: &mut Task) -> Result<()> {
    if task.reviewer.is_none() || (task.review_base.is_some() && task.review_head.is_some()) {
        return Ok(());
    }
    let reviewer = task.reviewer.as_ref().expect("checked reviewer");
    let id = reviewer.id.clone();
    store.save(task, "reviewer_stop_requested", json!({"run_id": id}))?;
    runtime::stop(config, task.reviewer.as_ref().expect("persisted reviewer")).await?;
    task.reviewer = None;
    store.save(task, "reviewer_stopped", json!({"run_id": id}))
}

#[derive(Deserialize)]
struct ApiCheck {
    id: u64,
    name: String,
    head_sha: String,
    status: String,
    conclusion: Option<String>,
    app: Option<Value>,
}
#[derive(Deserialize)]
struct ApiStatus {
    id: u64,
    context: String,
    state: String,
}

enum Gate {
    Passed(Value),
    Waiting(String),
    Failed(String),
}

async fn check_reports(config: &Config, commit: &str) -> Result<(Vec<ApiCheck>, Vec<ApiStatus>)> {
    let checks = paged(
        api(
            config,
            &format!("commits/{commit}/check-runs"),
            &[("filter", "latest"), ("per_page", "100")],
            true,
        )
        .await?,
        Some("check_runs"),
    )?;
    let statuses = paged(
        api(
            config,
            &format!("commits/{commit}/statuses"),
            &[("per_page", "100")],
            true,
        )
        .await?,
        None,
    )?;
    Ok((checks, statuses))
}

async fn pr_checks(config: &Config, pr: &ApiPull) -> Result<Gate> {
    let required = &config
        .github
        .as_ref()
        .context("GitHub configuration missing")?
        .required_checks;
    // GitHub uses test-merge checks when that commit has reports; head-only
    // checking would ignore a failed integration test on refs/pull/N/merge.
    if let Some(merge) = pr.merge_commit_sha.as_deref().filter(|sha| valid_sha(sha)) {
        let (checks, statuses) = check_reports(config, merge).await?;
        if !checks.is_empty() || !statuses.is_empty() {
            let commit = api(config, &format!("git/commits/{merge}"), &[], false).await?;
            let parents = commit
                .get("parents")
                .and_then(Value::as_array)
                .context("test merge has no parents")?;
            ensure!(
                parents.len() == 2
                    && parents[0].get("sha").and_then(Value::as_str) == Some(pr.base.sha.as_str())
                    && parents[1].get("sha").and_then(Value::as_str) == Some(pr.head.sha.as_str()),
                "test merge reports do not belong to the current PR base/head; wait for GitHub to refresh the merge ref"
            );
            return evaluate_pr_checks(required, merge, checks, statuses);
        }
    }
    let (checks, statuses) = check_reports(config, &pr.head.sha).await?;
    evaluate_pr_checks(required, &pr.head.sha, checks, statuses)
}

fn recorded_ci_commit(
    store: &Store,
    task: &Task,
    base: &str,
    head: &str,
) -> Result<Option<String>> {
    Ok(store.events(&task.id)?.iter().rev().find_map(|event| {
        if !matches!(
            event.kind.as_str(),
            "pr_ci_passed" | "pr_ci_reconfirmed" | "pr_ci_before_merge"
        ) {
            return None;
        }
        if event.data.get("base_sha").and_then(Value::as_str) != Some(base)
            || event.data.get("head_sha").and_then(Value::as_str) != Some(head)
        {
            return None;
        }
        event
            .data
            .pointer("/evidence/head_sha")
            .and_then(Value::as_str)
            .filter(|sha| valid_sha(sha))
            .map(str::to_owned)
    }))
}

fn evaluate_pr_checks(
    required_checks: &[String],
    head: &str,
    checks: Vec<ApiCheck>,
    statuses: Vec<ApiStatus>,
) -> Result<Gate> {
    ensure!(
        !required_checks.is_empty(),
        "Required check allowlist is empty"
    );
    let mut latest_checks: BTreeMap<(String, u64), ApiCheck> = BTreeMap::new();
    for check in checks {
        ensure!(
            check.head_sha == head,
            "GitHub returned a check run for a different head SHA"
        );
        let app = check
            .app
            .as_ref()
            .and_then(|app| app.get("id"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let key = (check.name.clone(), app);
        if latest_checks.get(&key).is_none_or(|old| check.id > old.id) {
            latest_checks.insert(key, check);
        }
    }
    let mut latest_statuses: BTreeMap<String, ApiStatus> = BTreeMap::new();
    for status in statuses {
        if latest_statuses
            .get(&status.context)
            .is_none_or(|old| status.id > old.id)
        {
            latest_statuses.insert(status.context.clone(), status);
        }
    }
    let mut missing = Vec::new();
    let mut pending = Vec::new();
    let mut failed = Vec::new();
    for required in required_checks {
        let matches: Vec<&ApiCheck> = latest_checks
            .iter()
            .filter_map(|((name, _), check)| (name == required).then_some(check))
            .collect();
        let status = latest_statuses.get(required);
        if matches.is_empty() && status.is_none() {
            missing.push(required.as_str());
        }
        for check in matches {
            if check.status != "completed" {
                pending.push(format!("{} ({})", required, check.status));
            } else if check.conclusion.as_deref() != Some("success") {
                failed.push(format!(
                    "{} ({})",
                    required,
                    check.conclusion.as_deref().unwrap_or("no conclusion")
                ));
            }
        }
        if let Some(status) = status {
            match status.state.as_str() {
                "success" => {}
                "pending" => pending.push(format!("{} (pending status)", required)),
                other => failed.push(format!("{} (status {})", required, other)),
            }
        }
    }
    if !failed.is_empty() {
        return Ok(Gate::Failed(format!(
            "Required checks failed on {head}: {}",
            failed.join(", ")
        )));
    }
    if !missing.is_empty() || !pending.is_empty() {
        return Ok(Gate::Waiting(format!(
            "Waiting for required checks on exact head {head}. Missing: [{}]. Pending: [{}]. Check required_checks spelling and ensure those CI jobs actually run for this PR; absent/skipped checks never qualify.",
            missing.join(", "),
            pending.join(", ")
        )));
    }
    let check_evidence = latest_checks.values().filter(|check| required_checks.contains(&check.name)).map(|check| {
        json!({"id": check.id, "name": check.name, "head_sha": check.head_sha, "status": check.status, "conclusion": check.conclusion})
    }).collect::<Vec<_>>();
    let status_evidence = latest_statuses
        .values()
        .filter(|status| required_checks.contains(&status.context))
        .map(|status| json!({"id": status.id, "context": status.context, "state": status.state}))
        .collect::<Vec<_>>();
    Ok(Gate::Passed(
        json!({"head_sha": head, "checks": check_evidence, "statuses": status_evidence}),
    ))
}

async fn review(config: &Config, store: &Store, task: &mut Task, pr: &ApiPull) -> Result<()> {
    if task.review_base.as_deref() != Some(pr.base.sha.as_str())
        || task.review_head.as_deref() != Some(pr.head.sha.as_str())
    {
        task.review_base = Some(pr.base.sha.clone());
        task.review_head = Some(pr.head.sha.clone());
        task.review_report = None;
        task.merge_authorized_head = None;
        task.state = TaskState::Review;
        task.last_error = None;
        store.save(
            task,
            "review_launch_requested",
            json!({"base_sha": pr.base.sha, "head_sha": pr.head.sha}),
        )?;
    }
    if task.reviewer.is_none() {
        let head_ref = format!("refs/int-agent/tasks/{}/review-head", task.id);
        let base_ref = format!("refs/int-agent/tasks/{}/review-base", task.id);
        let head_fetch = format!("+refs/pull/{}/head:{head_ref}", pr.number);
        let base_fetch = format!("+refs/heads/{}:{base_ref}", config.default_branch);
        command::run(
            "git",
            &[
                "fetch",
                "--no-tags",
                "--",
                &config.remote,
                &head_fetch,
                &base_fetch,
            ],
            &config.repository,
        )
        .await?;
        let fetched_head = command::run(
            "git",
            &["rev-parse", "--verify", &head_ref],
            &config.repository,
        )
        .await?;
        let fetched_base = command::run(
            "git",
            &["rev-parse", "--verify", &base_ref],
            &config.repository,
        )
        .await?;
        ensure!(
            fetched_head.trim() == pr.head.sha && fetched_base.trim() == pr.base.sha,
            "PR head/base changed while fetching review commits; waiting for rediscovery rather than reviewing stale refs"
        );
        let reviewer = runtime::start_reviewer(config, task, &pr.base.sha, &pr.head.sha).await?;
        task.reviewer = Some(reviewer);
        task.state = TaskState::Review;
        let data = json!({"run": task.reviewer, "base_sha": pr.base.sha, "head_sha": pr.head.sha});
        store.save(task, "reviewer_started", data)?;
    }
    let path = runtime::report_path(task.reviewer.as_ref().expect("launched reviewer"));
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => Some(file),
        Err(error) if error.kind() == ErrorKind::NotFound => None,
        Err(error) => return Err(error).context("Opening independent review report"),
    };
    if let Some(file) = file {
        let mut bytes = Vec::new();
        file.take(REPORT_LIMIT + 1).read_to_end(&mut bytes).await?;
        if bytes.len() as u64 > REPORT_LIMIT {
            return waiting(
                store,
                task,
                format!(
                    "Reviewer report {} exceeds 1 MiB; write the required bounded JSON report atomically.",
                    path.display()
                ),
            );
        }
        let report: ReviewReport = match serde_json::from_slice(&bytes) {
            Ok(report) => report,
            Err(error) => {
                return waiting(
                    store,
                    task,
                    format!(
                        "Waiting for a complete valid atomic Reviewer JSON report at {}: {}",
                        path.display(),
                        error
                    ),
                );
            }
        };
        if let Err(error) = validate_review_report(&report, &pr.base.sha, &pr.head.sha) {
            return waiting(
                store,
                task,
                format!(
                    "Rejected independent Reviewer report at {}: {error:#}. No PASS or authorization was accepted; correct the atomic report in the independent Reviewer session.",
                    path.display()
                ),
            );
        }
        task.state = match report.verdict {
            ReviewVerdict::Pass => TaskState::ReadyToMerge,
            ReviewVerdict::ChangesRequested => TaskState::ChangesRequested,
        };
        task.last_error = None;
        task.review_report = Some(report);
        let data = json!({"report": task.review_report});
        return store.save(task, "review_report_received", data);
    }
    let reviewer = task.reviewer.as_mut().expect("launched reviewer");
    let before = (reviewer.pid, reviewer.session_file.clone());
    let status = runtime::refresh(config, reviewer).await?;
    if before != (reviewer.pid, reviewer.session_file.clone()) {
        let data = json!({"run": task.reviewer});
        store.save(task, "reviewer_observed", data)?;
    }
    let message = if !status.alive {
        format!(
            "Independent Reviewer exited before writing {}. Inspect its preserved session and resume the Reviewer; no review result was inferred.",
            path.display()
        )
    } else if status.pending.is_some() {
        format!(
            "Independent Reviewer is awaiting human input in its native tmux window. Answer there; expected report: {}.",
            path.display()
        )
    } else if !status.ready {
        "Waiting for actual independent Reviewer omp session_start readiness, not merely tmux launch.".into()
    } else {
        format!(
            "Independent Reviewer is running for base {} / head {}; waiting for atomic JSON report at {}.",
            pr.base.sha,
            pr.head.sha,
            path.display()
        )
    };
    waiting(store, task, message)
}

fn validate_review_report(report: &ReviewReport, base: &str, head: &str) -> Result<()> {
    ensure!(
        report.base_sha == base && report.head_sha == head,
        "stale report: expected base {base} / head {head}, got base {} / head {}",
        report.base_sha,
        report.head_sha
    );
    ensure!(
        !report.summary.trim().is_empty(),
        "report summary must not be empty"
    );
    ensure!(
        !report.verification.is_empty()
            && report
                .verification
                .iter()
                .all(|item| !item.trim().is_empty()),
        "nonempty verification entries are required; a natural-language PASS is insufficient"
    );
    Ok(())
}

async fn publish_review(config: &Config, store: &Store, task: &mut Task) -> Result<()> {
    let report = task
        .review_report
        .as_ref()
        .context("Publishing review requires the persisted report")?;
    let key = format!("{}:{}", report.base_sha, report.head_sha);
    if event_seen(store, task, "review_comment_published", &key)? {
        return Ok(());
    }
    let number = task.pr.as_ref().context("Review has no PR")?.number;
    let marker = format!(
        "<!-- int-agent-review:{}:{}:{} -->",
        task.id, report.base_sha, report.head_sha
    );
    let comments: Vec<Value> = paged(
        api(
            config,
            &format!("issues/{number}/comments"),
            &[("per_page", "100")],
            true,
        )
        .await?,
        None,
    )?;
    let data = json!({"key": key, "base_sha": report.base_sha, "head_sha": report.head_sha});
    let already_published = comments.iter().any(|comment| {
        comment
            .get("body")
            .and_then(Value::as_str)
            .is_some_and(|body| body.contains(&marker))
    });
    let body = if already_published {
        None
    } else {
        let mut body = format!(
            "{marker}\nIndependent interactive omp Reviewer: **PASS**\n\nExact base: `{}`\nExact head: `{}`\n\n{}\n\nVerification:\n{}\n\nFindings:\n{}\n\nThis is an independent agent report, not a GitHub review approval or a claim that branch protection was bypassed. Full structured report is preserved with task `{}`.",
            report.base_sha,
            report.head_sha,
            report.summary,
            report
                .verification
                .iter()
                .map(|item| format!("- {item}"))
                .collect::<Vec<_>>()
                .join("\n"),
            if report.findings.is_empty() {
                "None.".into()
            } else {
                report.findings.join("\n")
            },
            task.id
        );
        if body.len() > 60_000 {
            let mut end = 59_000;
            while !body.is_char_boundary(end) {
                end -= 1;
            }
            body.truncate(end);
            body.push_str("\n\n[Comment shortened; the complete structured report is retained in the task record. This is not a GitHub approval.]");
        }
        Some(body)
    };
    if let Some(body) = body {
        if !event_seen(store, task, "review_comment_requested", &key)? {
            store.save(task, "review_comment_requested", data.clone())?;
        }
        let endpoint = format!("repos/{}/issues/{number}/comments", repository(config)?);
        let field = format!("body={body}");
        command::run(
            &config.gh,
            &["api", "--method", "POST", &endpoint, "-f", &field],
            &config.repository,
        )
        .await?;
    }
    store.save(task, "review_comment_published", data)
}

fn repair_lineage(store: &Store, task: &Task) -> Result<Vec<Task>> {
    let mut seen = BTreeSet::from([task.id.clone()]);
    let mut next = task.spec.repairs_task_id.clone();
    let mut lineage = Vec::new();
    while let Some(id) = next {
        ensure!(
            seen.insert(id.clone()),
            "Cycle in explicit integration repair lineage at task {id}; integration gate remains closed"
        );
        let original = store.get(&id).with_context(|| {
            format!("Missing task {id} in explicit repair lineage; integration gate remains closed")
        })?;
        next = original.spec.repairs_task_id.clone();
        lineage.push(original);
    }
    Ok(lineage)
}

fn merge_blocker(store: &Store, task: &Task) -> Result<Option<String>> {
    let lineage = repair_lineage(store, task)?;
    for other in store.list_active()? {
        if other.id == task.id {
            continue;
        }
        match other.state {
            TaskState::Merging | TaskState::PostMergeCi => {
                return Ok(Some(format!(
                    "Task {} owns the serial merge-through-push-CI gate ({:?}).",
                    other.id, other.state
                )));
            }
            TaskState::IntegrationBlocked
                if !lineage.iter().any(|ancestor| ancestor.id == other.id) =>
            {
                return Ok(Some(format!(
                    "Integration is globally blocked by task {}. Only a repair explicitly linked to that task or its blocked repair descendants may merge.",
                    other.id
                )));
            }
            _ => {}
        }
    }
    if let Some(original) = lineage.first()
        && original.state != TaskState::IntegrationBlocked
    {
        return Ok(Some(format!(
            "Linked repair target {} is not IntegrationBlocked ({:?}); refusing an unproven repair gate exception.",
            original.id, original.state
        )));
    }
    Ok(None)
}

fn resolve_repairs(store: &Store, task: &Task) -> Result<()> {
    if task.repaired_by.is_some() {
        return Ok(());
    }
    let lineage = repair_lineage(store, task)?;
    let mut goal_delivered = task.spec.completes_original_goal;
    for mut original in lineage {
        if original.state == TaskState::IntegrationBlocked {
            original.state = if goal_delivered {
                TaskState::Completed
            } else {
                TaskState::NeedsAttention
            };
            original.repaired_by = Some(task.id.clone());
            original.last_error = if goal_delivered {
                None
            } else {
                Some(format!(
                    "Main integration was restored by repair task {}, but the original task goal was not proven delivered. This task remains NeedsAttention and must not be archived as completed.",
                    task.id
                ))
            };
            // Original failed merge/CI evidence is immutable history. A repair
            // supplies its own success evidence, never rewrites the old run.
            store.save(
                &mut original,
                "integration_repaired",
                json!({
                    "repair_task_id": task.id, "repair_merge_sha": task.merge_sha,
                    "repair_post_ci": task.post_ci, "original_goal_delivered": goal_delivered,
                }),
            )?;
        } else {
            ensure!(
                original.repaired_by.as_deref() == Some(task.id.as_str())
                    && matches!(
                        original.state,
                        TaskState::Completed | TaskState::Archived | TaskState::NeedsAttention
                    ),
                "Repair ancestor {} is no longer blocked and was not restored by this repair; refusing to overwrite its evidence",
                original.id
            );
        }
        goal_delivered &= original.spec.completes_original_goal;
    }
    Ok(())
}

async fn live_base(config: &Config) -> Result<String> {
    let value = api(
        config,
        &format!("git/ref/heads/{}", encode(&config.default_branch)),
        &[],
        false,
    )
    .await?;
    let sha = value
        .pointer("/object/sha")
        .and_then(Value::as_str)
        .context("GitHub default-branch ref has no SHA")?;
    ensure!(valid_sha(sha), "GitHub default-branch ref has invalid SHA");
    Ok(sha.to_owned())
}

async fn begin_merge(config: &Config, store: &Store, task: &mut Task) -> Result<()> {
    let report = task
        .review_report
        .as_ref()
        .context("ReadyToMerge requires an independent review report")?;
    ensure!(
        report.verdict == ReviewVerdict::Pass,
        "ReadyToMerge requires Reviewer PASS"
    );
    let github = config.github.as_ref().context("GitHub config missing")?;
    if github.merge_policy == MergePolicy::Manual
        && task.merge_authorized_head.as_deref() != Some(report.head_sha.as_str())
    {
        let message = format!(
            "PR is independently reviewed at exact head {}. Manual merge policy requires team_authorize_merge with expected_head={}.",
            report.head_sha, report.head_sha
        );
        return waiting(store, task, message);
    }
    if let Some(reason) = merge_blocker(store, task)? {
        return waiting(store, task, reason);
    }
    let pr = get_pr(
        config,
        task.pr.as_ref().context("ReadyToMerge has no PR")?.number,
    )
    .await?;
    validate_pr(config, task, &pr)?;
    if pr.merged {
        return record_merged(config, store, task, &pr, true).await;
    }
    if pr.head.sha != report.head_sha
        || pr.base.sha != report.base_sha
        || live_base(config).await? != report.base_sha
    {
        invalidate_review(
            store,
            task,
            &pr,
            "Live GitHub head/default-branch base changed immediately before merge; review and authorization invalidated",
        )?;
        return retire_stale_reviewer(config, store, task).await;
    }
    if pr.state != "open" || pr.draft {
        return waiting(
            store,
            task,
            format!(
                "PR #{} is not an open, non-draft merge candidate.",
                pr.number
            ),
        );
    }
    match pr_checks(config, &pr).await? {
        Gate::Passed(evidence) => store.save(
            task,
            "pr_ci_before_merge",
            json!({"base_sha":pr.base.sha,"head_sha":pr.head.sha,"evidence":evidence}),
        )?,
        Gate::Waiting(reason) | Gate::Failed(reason) => return waiting(store, task, reason),
    }
    // The durable exclusive state precedes the first externally visible merge
    // request. A crash can therefore only leave a lock to reconcile, not an
    // untracked merge while another task is admitted.
    task.state = TaskState::Merging;
    task.last_error = None;
    store.save(task, "merge_requested", json!({"pr": pr.number, "base_sha": pr.base.sha, "head_sha": pr.head.sha, "policy": github.merge_policy}))?;
    submit_merge(config, store, task, &pr).await
}

async fn submit_merge(config: &Config, store: &Store, task: &mut Task, pr: &ApiPull) -> Result<()> {
    let number = pr.number.to_string();
    store.save(
        task,
        "merge_command_started",
        json!({"pr": pr.number, "head_sha": pr.head.sha, "base_sha": pr.base.sha}),
    )?;
    let output = command::output(
        &config.gh,
        &[
            "pr",
            "merge",
            &number,
            "--repo",
            repository(config)?,
            "--merge",
            "--match-head-commit",
            &pr.head.sha,
        ],
        &config.repository,
    )
    .await?;
    store.save(task, "merge_command_returned", json!({"pr": pr.number, "head_sha": pr.head.sha, "code": output.code, "stdout": output.stdout, "stderr": output.stderr}))?;
    // An error/timeout is not proof that GitHub did not perform the merge.
    let actual = get_pr(config, pr.number).await?;
    validate_pr(config, task, &actual)?;
    if actual.merged {
        return record_merged(config, store, task, &actual, false).await;
    }
    if output.code != 0 {
        bail!(
            "Merge command failed but PR is not confirmed merged; task retains the exclusive Merging state for reconciliation. gh: {}",
            output.stderr.trim()
        );
    }
    waiting(
        store,
        task,
        format!(
            "Merge request for PR #{} returned successfully, but GitHub has not yet confirmed merged=true and a merge commit. Exclusive Merging gate is retained.",
            pr.number
        ),
    )
}

async fn reconcile_merge(config: &Config, store: &Store, task: &mut Task) -> Result<()> {
    let number = task
        .pr
        .as_ref()
        .context("Merging task has no persisted PR identity")?
        .number;
    let pr = get_pr(config, number).await?;
    validate_pr(config, task, &pr)?;
    if pr.merged {
        return record_merged(config, store, task, &pr, false).await;
    }
    if task.merge_sha.is_some() {
        return waiting(store, task, "A real merge SHA was already observed, but the current GitHub response does not confirm the merge. Exclusive gate and prior evidence are retained; inspect GitHub/API consistency rather than releasing integration.".into());
    }
    if pr.state != "open" {
        return waiting(
            store,
            task,
            format!(
                "Reconciling PR #{number}: closed but not confirmed merged. Exclusive Merging gate retained; inspect GitHub before reopening/resuming."
            ),
        );
    }
    let submitted = store.events(&task.id)?.iter().any(|event| {
        event.kind == "merge_command_returned"
            && event.data.get("head_sha").and_then(Value::as_str) == task.review_head.as_deref()
            && event.data.get("code").and_then(Value::as_i64) == Some(0)
    });
    if submitted {
        return waiting(
            store,
            task,
            format!(
                "GitHub accepted the merge command for PR #{number}; waiting for merged=true and merge_commit_sha without submitting a duplicate merge."
            ),
        );
    }
    let report = task
        .review_report
        .as_ref()
        .context("Merging task has no persisted review report; exclusive lock retained")?;
    if pr.head.sha != report.head_sha
        || pr.base.sha != report.base_sha
        || live_base(config).await? != report.base_sha
    {
        // An authoritative open PR without an accepted merge request can be
        // re-reviewed. An accepted/observed merge above never releases its lock.
        invalidate_review(
            store,
            task,
            &pr,
            "Open PR changed while reconciling merge; restarting exact-SHA CI and independent review",
        )?;
        return retire_stale_reviewer(config, store, task).await;
    }
    if report.verdict != ReviewVerdict::Pass
        || task.review_base.as_deref() != Some(pr.base.sha.as_str())
        || task.review_head.as_deref() != Some(pr.head.sha.as_str())
    {
        bail!("Merging task lacks a matching independent PASS; exclusive lock retained");
    }
    let github = config.github.as_ref().context("GitHub config missing")?;
    if github.merge_policy == MergePolicy::Manual
        && task.merge_authorized_head.as_deref() != Some(pr.head.sha.as_str())
    {
        return waiting(store, task, "Merging reconciliation requires the original exact-head manual authorization; exclusive gate retained.".into());
    }
    if let Some(reason) = merge_blocker(store, task)? {
        return waiting(store, task, reason);
    }
    match pr_checks(config, &pr).await? {
        Gate::Passed(evidence) => {
            store.save(
                task,
                "pr_ci_reconfirmed",
                json!({"base_sha": pr.base.sha, "head_sha": pr.head.sha, "evidence": evidence}),
            )?;
            submit_merge(config, store, task, &pr).await
        }
        Gate::Waiting(reason) | Gate::Failed(reason) => waiting(
            store,
            task,
            format!("Merge reconciliation is holding its exclusive gate: {reason}"),
        ),
    }
}

async fn record_merged(
    config: &Config,
    store: &Store,
    task: &mut Task,
    pr: &ApiPull,
    external: bool,
) -> Result<()> {
    let Some(merge_sha) = pr.merge_commit_sha.as_ref().filter(|sha| valid_sha(sha)) else {
        if task.state != TaskState::Merging {
            task.state = TaskState::Merging;
            task.pr = Some(snapshot(pr));
            store.save(
                task,
                "merged_identity_pending",
                json!({"pr": pr.number, "external": external}),
            )?;
        }
        return waiting(
            store,
            task,
            format!(
                "PR #{} is merged, but GitHub has not provided a valid merge_commit_sha. Exclusive integration gate retained.",
                pr.number
            ),
        );
    };
    if task.merge_sha.as_ref().is_some_and(|old| old != merge_sha) {
        return block_integration(config, store, task, "GitHub merge commit differs from the persisted merge identity; refusing to replace integration evidence.".into()).await;
    }
    let reviewed = task.review_report.as_ref().is_some_and(|report| {
        report.verdict == ReviewVerdict::Pass
            && report.head_sha == pr.head.sha
            && task.review_base.as_deref() == Some(report.base_sha.as_str())
            && task.review_head.as_deref() == Some(report.head_sha.as_str())
            && validate_review_report(report, &report.base_sha, &pr.head.sha).is_ok()
    });
    let changed = task.state != TaskState::Merging || task.merge_sha.is_none();
    task.pr = Some(snapshot(pr));
    task.merge_sha = Some(merge_sha.clone());
    task.state = TaskState::Merging;
    if changed {
        store.save(task, "merge_observed", json!({"pr": pr.number, "merge_sha": merge_sha, "external": external, "matched_review": reviewed}))?;
    }
    if !reviewed {
        return block_integration(config, store, task, format!(
            "PR #{} really merged as {}, but no matching persisted independent exact-head PASS exists. The review gate was bypassed; preserving integration evidence and requiring an explicitly linked repair rather than claiming completion.",
            pr.number, merge_sha
        )).await;
    }
    let github = config
        .github
        .as_ref()
        .context("Merged task requires GitHub configuration")?;
    if github.merge_policy == MergePolicy::Manual
        && task.merge_authorized_head.as_deref() != Some(pr.head.sha.as_str())
    {
        return block_integration(config, store, task, "PR was merged without its persisted exact-head manual authorization; merge evidence is retained and a linked repair is required.".into()).await;
    }
    let commit = api(config, &format!("git/commits/{merge_sha}"), &[], false).await?;
    if let Err(error) = validate_merge_parents(
        task.review_report.as_ref().expect("verified report"),
        &commit,
    ) {
        return block_integration(config, store, task, format!("Actual merge commit invalidates the independent review basis: {error:#}. A linked repair is required; a stale-base PASS cannot complete this task.")).await;
    }
    let base = task
        .review_base
        .as_deref()
        .context("merged task has no review base")?;
    let Some(ci_commit) = recorded_ci_commit(store, task, base, &pr.head.sha)? else {
        return block_integration(config, store, task, "Merged task has no persisted successful PR-CI evidence for its reviewed base/head; a linked repair is required.".into()).await;
    };
    let (checks, statuses) = check_reports(config, &ci_commit).await?;
    match evaluate_pr_checks(&github.required_checks, &ci_commit, checks, statuses)? {
        Gate::Passed(evidence) => {
            let data = json!({"base_sha": task.review_base, "head_sha": pr.head.sha, "evidence": evidence});
            store.save(task, "pr_ci_at_merge", data)?;
        }
        Gate::Waiting(reason) => {
            return waiting(
                store,
                task,
                format!(
                    "Real merge {merge_sha} is recorded and the exclusive gate is held, but required exact-head PR CI is still missing/pending: {reason}"
                ),
            );
        }
        Gate::Failed(reason) => {
            return block_integration(
                config,
                store,
                task,
                format!("Merged PR does not satisfy required exact-head PR CI: {reason}"),
            )
            .await;
        }
    }
    publish_review(config, store, task).await?;
    task.state = TaskState::PostMergeCi;
    task.last_error = None;
    store.save(task, "merge_confirmed", json!({"pr": pr.number, "merge_sha": merge_sha, "external": external, "matched_review": true}))?;
    post_merge_ci(config, store, task).await
}

fn validate_merge_parents(report: &ReviewReport, commit: &Value) -> Result<()> {
    let parents = commit
        .get("parents")
        .and_then(Value::as_array)
        .context("GitHub merge commit has no parent identities")?;
    ensure!(
        parents.len() == 2,
        "actual commit is not the required two-parent merge commit"
    );
    ensure!(
        parents[0].get("sha").and_then(Value::as_str) == Some(report.base_sha.as_str()),
        "actual first parent is not independently reviewed base {}",
        report.base_sha
    );
    ensure!(
        parents[1].get("sha").and_then(Value::as_str) == Some(report.head_sha.as_str()),
        "actual second parent is not independently reviewed head {}",
        report.head_sha
    );
    Ok(())
}

#[derive(Deserialize)]
struct ApiWorkflowRun {
    id: u64,
    run_number: u64,
    run_attempt: Option<u64>,
    head_sha: String,
    head_branch: Option<String>,
    event: String,
    status: String,
    conclusion: Option<String>,
    html_url: String,
}
#[derive(Deserialize)]
struct ApiJob {
    id: u64,
    run_id: u64,
    run_attempt: Option<u64>,
    head_sha: String,
    name: String,
    status: String,
    conclusion: Option<String>,
    steps: Option<Vec<ApiStep>>,
}
#[derive(Deserialize)]
struct ApiStep {
    name: String,
    status: String,
    conclusion: Option<String>,
}

struct JobObservation {
    failures: Vec<String>,
    waits: Vec<String>,
    observed_jobs: Vec<Value>,
}

fn evaluate_jobs(run: &ApiWorkflowRun, jobs: Vec<ApiJob>) -> Result<JobObservation> {
    let attempt = run.run_attempt.unwrap_or(1);
    let mut failures = Vec::new();
    let mut waits = Vec::new();
    let mut observed_jobs = Vec::new();
    if run.status != "completed" {
        waits.push(format!("workflow {}", run.status));
    } else if run.conclusion.as_deref() != Some("success") {
        failures.push(format!(
            "workflow conclusion {}",
            run.conclusion.as_deref().unwrap_or("missing")
        ));
    }
    if jobs.is_empty() {
        waits.push("no real jobs reported; an empty run is not CI evidence".into());
    }
    for job in jobs {
        ensure!(
            job.run_id == run.id
                && job.head_sha == run.head_sha
                && job.run_attempt.is_none_or(|value| value == attempt),
            "Workflow job identity does not match the exact selected merge-SHA run/attempt"
        );
        if job.status != "completed" {
            waits.push(format!("job {} is {}", job.name, job.status));
        } else if job.conclusion.as_deref() != Some("success") {
            failures.push(format!(
                "job {} concluded {}",
                job.name,
                job.conclusion.as_deref().unwrap_or("missing")
            ));
        }
        let mut successful_steps = 0usize;
        if let Some(steps) = &job.steps {
            for step in steps {
                if step.conclusion.as_deref() == Some("skipped") {
                    continue;
                }
                if step.status != "completed" {
                    waits.push(format!(
                        "job {} step {} is {}",
                        job.name, step.name, step.status
                    ));
                } else if step.conclusion.as_deref() == Some("success") {
                    successful_steps += 1;
                } else {
                    failures.push(format!(
                        "job {} step {} concluded {}",
                        job.name,
                        step.name,
                        step.conclusion.as_deref().unwrap_or("missing")
                    ));
                }
            }
            if successful_steps == 0 {
                waits.push(format!("job {} has no executed successful steps", job.name));
            }
        }
        observed_jobs.push(json!({"id": job.id, "name": job.name, "status": job.status, "conclusion": job.conclusion, "run_attempt": attempt, "successful_steps": successful_steps}));
    }
    Ok(JobObservation {
        failures,
        waits,
        observed_jobs,
    })
}

fn latest_merge_run(runs: Vec<ApiWorkflowRun>, sha: &str, branch: &str) -> Option<ApiWorkflowRun> {
    runs.into_iter()
        .filter(|run| {
            run.head_sha == sha && run.head_branch.as_deref() == Some(branch) && run.event == "push"
        })
        .max_by_key(|run| (run.run_number, run.id, run.run_attempt.unwrap_or(1)))
}

async fn post_merge_ci(config: &Config, store: &Store, task: &mut Task) -> Result<()> {
    let github = config
        .github
        .as_ref()
        .context("PostMergeCi requires GitHub configuration")?;
    let sha = task
        .merge_sha
        .clone()
        .context("PostMergeCi requires a confirmed merge SHA")?;
    ensure!(valid_sha(&sha), "Invalid persisted merge SHA");
    let mut pending = Vec::new();
    for workflow in &github.post_merge_workflows {
        let runs: Vec<ApiWorkflowRun> = paged(
            api(
                config,
                &format!("actions/workflows/{}/runs", encode(workflow)),
                &[
                    ("head_sha", &sha),
                    ("event", "push"),
                    ("branch", &config.default_branch),
                    ("per_page", "100"),
                ],
                true,
            )
            .await?,
            Some("workflow_runs"),
        )?;
        let run = latest_merge_run(runs, &sha, &config.default_branch);
        let Some(run) = run else {
            pending.push(format!("{workflow}: no push run for exact merge SHA {sha} on {} (ensure the configured workflow triggers on push to this branch)", config.default_branch));
            continue;
        };
        let attempt = run.run_attempt.unwrap_or(1);
        let jobs: Vec<ApiJob> =
            if run.status == "completed" && run.conclusion.as_deref() != Some("success") {
                // A known workflow failure must be persisted even if a later jobs
                // query would fail. Real failure evidence is already sufficient.
                Vec::new()
            } else {
                paged(
                    api(
                        config,
                        &format!("actions/runs/{}/attempts/{attempt}/jobs", run.id),
                        &[("per_page", "100")],
                        true,
                    )
                    .await?,
                    Some("jobs"),
                )?
            };
        let JobObservation {
            failures,
            waits,
            observed_jobs,
        } = evaluate_jobs(&run, jobs)?;
        let conclusion = if !failures.is_empty() {
            "failure".to_owned()
        } else if !waits.is_empty() {
            format!("waiting: {}", waits.join("; "))
        } else {
            "success".to_owned()
        };
        let evidence = CiEvidence {
            workflow: workflow.clone(),
            run_id: run.id,
            head_sha: sha.clone(),
            url: run.html_url,
            conclusion,
        };
        let changed = task
            .post_ci
            .iter()
            .find(|old| old.workflow == *workflow)
            .is_none_or(|old| {
                old.run_id != evidence.run_id
                    || old.head_sha != evidence.head_sha
                    || old.conclusion != evidence.conclusion
                    || old.url != evidence.url
            });
        let observation = changed
            .then(|| json!({"evidence": evidence, "run_attempt": attempt, "jobs": observed_jobs}));
        task.post_ci.retain(|old| old.workflow != *workflow);
        task.post_ci.push(evidence);
        if let Some(data) = observation {
            store.save(task, "post_merge_ci_observed", data)?;
        }
        if !failures.is_empty() {
            return block_integration(config, store, task, format!("Exact merge-SHA push CI failed: workflow {workflow}, run {}, attempt {attempt}, commit {sha}: {}", run.id, failures.join("; "))).await;
        }
        if !waits.is_empty() {
            pending.push(format!(
                "{workflow}, run {} attempt {attempt}: {}",
                run.id,
                waits.join("; ")
            ));
        }
    }
    if !pending.is_empty() {
        return waiting(
            store,
            task,
            format!(
                "Post-merge integration gate is held for exact commit {sha}. {}",
                pending.join(" | ")
            ),
        );
    }
    task.state = TaskState::Completed;
    task.last_error = None;
    let data = json!({"merge_sha": sha, "post_ci": task.post_ci});
    store.save(task, "integration_completed", data)?;
    finish_and_archive(config, store, task).await
}

async fn block_integration(
    config: &Config,
    store: &Store,
    task: &mut Task,
    reason: String,
) -> Result<()> {
    task.state = TaskState::IntegrationBlocked;
    task.last_error = Some(reason.clone());
    let data = json!({"reason": reason, "merge_sha": task.merge_sha, "post_ci": task.post_ci});
    store.save(task, "integration_blocked", data)?;
    let key = format!(
        "integration:{}",
        task.merge_sha.as_deref().unwrap_or("unknown")
    );
    let message = format!(
        "GLOBAL INTEGRATION BLOCK: {reason}\nTask {} is not complete. Preserve its branch, sessions and failure evidence. Diagnose the failure and tell the steward/user to start a dedicated repair task with repairs_task_id={}. Only that explicitly linked repair may pass the serial merge gate; no unrelated task may merge until its exact merge-SHA push CI succeeds.",
        task.id, task.id
    );
    feedback(config, store, task, &key, &message).await
}

async fn finish_and_archive(config: &Config, store: &Store, task: &mut Task) -> Result<()> {
    let sha = task
        .merge_sha
        .clone()
        .context("Completed task has no real confirmed merge SHA; refusing archive")?;
    resolve_repairs(store, task)?;
    let key = format!("archive:{sha}");
    if !event_seen(store, task, "archive_requested", &key)? {
        let data =
            json!({"key": key, "merge_sha": task.merge_sha, "repaired_by": task.repaired_by});
        store.save(task, "archive_requested", data)?;
    }
    runtime::archive(config, task)
        .await
        .context("Safe archive refused; Completed task and all evidence retained")?;
    delete_remote_branch(config, store, task).await?;
    task.state = TaskState::Archived;
    task.last_error = None;
    let data = json!({"merge_sha": task.merge_sha, "repaired_by": task.repaired_by, "worktree": task.worktree, "worker": task.worker, "reviewer": task.reviewer});
    store.save(task, "archived", data)
}

async fn delete_remote_branch(config: &Config, store: &Store, task: &mut Task) -> Result<()> {
    let Some(github) = &config.github else {
        return Ok(());
    };
    if !github.delete_remote_branches {
        return Ok(());
    }
    ensure!(
        task.branch != config.default_branch,
        "Refusing to delete the default branch"
    );
    let expected = task
        .pr
        .as_ref()
        .context("Remote branch cleanup requires the merged PR identity")?
        .head_sha
        .clone();
    let reference = format!("refs/heads/{}", task.branch);
    let refs = api(
        config,
        &format!("git/matching-refs/heads/{}", encode(&task.branch)),
        &[],
        false,
    )
    .await?;
    let refs = refs
        .as_array()
        .context("GitHub matching refs response is not an array")?;
    let matching = refs
        .iter()
        .find(|value| value.get("ref").and_then(Value::as_str) == Some(reference.as_str()));
    let Some(value) = matching else {
        return Ok(());
    };
    ensure!(
        value.pointer("/object/sha").and_then(Value::as_str) == Some(expected.as_str()),
        "Remote task branch changed after the merged PR head; preserving it instead of deleting new work"
    );
    let key = format!("delete-branch:{expected}");
    if !event_seen(store, task, "remote_branch_delete_requested", &key)? {
        let data = json!({"key": key, "branch": task.branch, "expected_head": expected});
        store.save(task, "remote_branch_delete_requested", data)?;
    }
    // GitHub's DELETE-ref API has no expected-SHA precondition. Git's lease
    // closes the race between revalidation and deletion without force-deleting
    // a branch somebody advanced while the archive was being performed.
    let lease = format!("--force-with-lease={reference}:{expected}");
    let deletion = format!(":{reference}");
    command::run(
        "git",
        &["push", &lease, "--", &config.remote, &deletion],
        &config.repository,
    )
    .await?;
    let data = json!({"key": key, "branch": task.branch, "expected_head": expected});
    store.save(task, "remote_branch_deleted", data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{StartTask, now};
    use std::path::PathBuf;

    struct TestStore {
        store: Option<Store>,
        _directory: tempfile::TempDir,
    }
    impl TestStore {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            Self {
                store: Some(Store::open(directory.path()).unwrap()),
                _directory: directory,
            }
        }
        fn get(&self) -> &Store {
            self.store.as_ref().unwrap()
        }
    }
    impl Drop for TestStore {
        fn drop(&mut self) {
            drop(self.store.take());
        }
    }

    fn task(id: &str, state: TaskState, repairs: Option<&str>, completes: bool) -> Task {
        let sha = "c".repeat(40);
        Task {
            id: id.into(),
            spec: StartTask {
                request_id: format!("request-{id}"),
                title: id.into(),
                goal: "Deliver the requested change".into(),
                non_goals: vec![],
                acceptance: vec!["Real CI and review succeed".into()],
                base_sha: "a".repeat(40),
                branch_slug: id.into(),
                model: None,
                repairs_task_id: repairs.map(str::to_owned),
                completes_original_goal: completes,
            },
            state,
            branch: format!("worker/{id}"),
            worktree: PathBuf::from(format!("/test/{id}")),
            worker: None,
            reviewer: None,
            pr: Some(PullRequest {
                number: 1,
                url: "https://github.com/owner/repo/pull/1".into(),
                head_sha: "b".repeat(40),
                base_sha: "a".repeat(40),
            }),
            review_base: None,
            review_head: None,
            review_report: None,
            merge_authorized_head: None,
            merge_sha: Some(sha.clone()),
            post_ci: vec![CiEvidence {
                workflow: "ci.yml".into(),
                run_id: 1,
                head_sha: sha,
                url: "https://github.com/owner/repo/actions/runs/1".into(),
                conclusion: if state == TaskState::IntegrationBlocked {
                    "failure"
                } else {
                    "success"
                }
                .into(),
            }],
            repaired_by: None,
            last_error: None,
            created_at: now(),
            updated_at: now(),
        }
    }

    fn check(id: u64, head: &str, conclusion: &str) -> ApiCheck {
        ApiCheck {
            id,
            name: "test".into(),
            head_sha: head.into(),
            status: "completed".into(),
            conclusion: Some(conclusion.into()),
            app: Some(json!({"id": 1})),
        }
    }

    #[test]
    fn absent_skipped_and_superseded_pr_checks_cannot_pass() {
        let head = "b".repeat(40);
        let required = vec!["test".into()];
        assert!(matches!(
            evaluate_pr_checks(&required, &head, vec![], vec![]).unwrap(),
            Gate::Waiting(_)
        ));
        for conclusion in ["skipped", "neutral", "cancelled", "failure", "timed_out"] {
            assert!(matches!(
                evaluate_pr_checks(&required, &head, vec![check(1, &head, conclusion)], vec![])
                    .unwrap(),
                Gate::Failed(_)
            ));
        }
        assert!(matches!(
            evaluate_pr_checks(
                &required,
                &head,
                vec![check(1, &head, "success"), check(2, &head, "failure")],
                vec![]
            )
            .unwrap(),
            Gate::Failed(_)
        ));
        assert!(
            evaluate_pr_checks(
                &required,
                &head,
                vec![check(1, &"d".repeat(40), "success")],
                vec![]
            )
            .is_err()
        );
        assert!(matches!(
            evaluate_pr_checks(&required, &head, vec![check(2, &head, "success")], vec![]).unwrap(),
            Gate::Passed(_)
        ));
    }

    fn report() -> ReviewReport {
        ReviewReport {
            base_sha: "a".repeat(40),
            head_sha: "b".repeat(40),
            verdict: ReviewVerdict::Pass,
            summary: "Independent review completed".into(),
            findings: vec![],
            verification: vec![
                "Inspected the exact base-to-head diff and acceptance evidence".into(),
            ],
        }
    }

    #[test]
    fn changed_base_or_head_invalidates_persisted_review_and_authorization() {
        let harness = TestStore::new();
        let store = harness.get();
        let mut item = task("feature", TaskState::ReadyToMerge, None, false);
        item.review_base = Some("a".repeat(40));
        item.review_head = Some("b".repeat(40));
        item.review_report = Some(report());
        item.merge_authorized_head = Some("b".repeat(40));
        store.insert(&item).unwrap();
        let changed = ApiPull {
            number: 1,
            html_url: "https://github.com/owner/repo/pull/1".into(),
            state: "open".into(),
            draft: false,
            merged: false,
            merge_commit_sha: None,
            head: ApiRef {
                sha: "b".repeat(40),
                branch: item.branch.clone(),
                repo: Some(ApiRepository {
                    full_name: "owner/repo".into(),
                }),
            },
            base: ApiRef {
                sha: "d".repeat(40),
                branch: "main".into(),
                repo: Some(ApiRepository {
                    full_name: "owner/repo".into(),
                }),
            },
        };
        assert!(validate_review_report(&report(), &changed.base.sha, &changed.head.sha).is_err());
        assert!(validate_review_report(&report(), &"a".repeat(40), &"e".repeat(40)).is_err());
        assert!(
            validate_merge_parents(
                &report(),
                &json!({"parents": [{"sha": "a".repeat(40)}, {"sha": "b".repeat(40)}]})
            )
            .is_ok()
        );
        assert!(
            validate_merge_parents(
                &report(),
                &json!({"parents": [{"sha": "d".repeat(40)}, {"sha": "b".repeat(40)}]})
            )
            .is_err()
        );
        assert!(
            validate_merge_parents(&report(), &json!({"parents": [{"sha": "a".repeat(40)}]}))
                .is_err()
        );
        adopt_pr(store, &mut item, &changed).unwrap();
        let persisted = store.get("feature").unwrap();
        assert_eq!(persisted.state, TaskState::PrCi);
        assert!(persisted.review_report.is_none());
        assert!(persisted.review_base.is_none() && persisted.review_head.is_none());
        assert!(persisted.merge_authorized_head.is_none());
    }

    fn workflow(id: u64, sha: &str, event: &str, branch: &str) -> ApiWorkflowRun {
        ApiWorkflowRun {
            id,
            run_number: id,
            run_attempt: Some(1),
            head_sha: sha.into(),
            head_branch: Some(branch.into()),
            event: event.into(),
            status: "completed".into(),
            conclusion: Some("success".into()),
            html_url: format!("https://github.com/owner/repo/actions/runs/{id}"),
        }
    }

    fn job(sha: &str, conclusion: &str) -> ApiJob {
        ApiJob {
            id: 10,
            run_id: 1,
            run_attempt: Some(1),
            head_sha: sha.into(),
            name: "test".into(),
            status: "completed".into(),
            conclusion: Some(conclusion.into()),
            steps: Some(vec![ApiStep {
                name: "Run tests".into(),
                status: "completed".into(),
                conclusion: Some("success".into()),
            }]),
        }
    }

    #[test]
    fn postmerge_requires_exact_push_sha_and_real_successful_jobs() {
        let sha = "c".repeat(40);
        assert!(
            latest_merge_run(
                vec![
                    workflow(2, &"d".repeat(40), "push", "main"),
                    workflow(3, &sha, "pull_request", "main"),
                    workflow(4, &sha, "push", "other"),
                ],
                &sha,
                "main"
            )
            .is_none()
        );
        let run = latest_merge_run(
            vec![
                workflow(1, &sha, "push", "main"),
                workflow(2, &sha, "pull_request", "main"),
            ],
            &sha,
            "main",
        )
        .unwrap();
        assert_eq!(run.id, 1);
        let empty = evaluate_jobs(&run, vec![]).unwrap();
        assert!(!empty.waits.is_empty());
        let skipped = evaluate_jobs(&run, vec![job(&sha, "skipped")]).unwrap();
        assert!(!skipped.failures.is_empty());
        let successful = evaluate_jobs(&run, vec![job(&sha, "success")]).unwrap();
        assert!(successful.failures.is_empty() && successful.waits.is_empty());
        let mut no_execution = job(&sha, "success");
        no_execution.steps.as_mut().unwrap()[0].conclusion = Some("skipped".into());
        assert!(
            !evaluate_jobs(&run, vec![no_execution])
                .unwrap()
                .waits
                .is_empty()
        );
        assert!(evaluate_jobs(&run, vec![job(&"d".repeat(40), "success")]).is_err());
    }

    #[test]
    fn chained_repairs_exclude_unrelated_merges_and_preserve_original_failure() {
        let harness = TestStore::new();
        let store = harness.get();
        let original = task("original", TaskState::IntegrationBlocked, None, false);
        let failed_repair = task(
            "failed-repair",
            TaskState::IntegrationBlocked,
            Some("original"),
            false,
        );
        let mut repair = task(
            "repair",
            TaskState::ReadyToMerge,
            Some("failed-repair"),
            true,
        );
        let unrelated = task("unrelated", TaskState::ReadyToMerge, None, false);
        for item in [&original, &failed_repair, &repair, &unrelated] {
            store.insert(item).unwrap();
        }
        assert!(merge_blocker(store, &unrelated).unwrap().is_some());
        assert!(merge_blocker(store, &repair).unwrap().is_none());
        let mut other = unrelated.clone();
        other.state = TaskState::PostMergeCi;
        store
            .save(&mut other, "test_transition", json!({}))
            .unwrap();
        assert!(merge_blocker(store, &repair).unwrap().is_some());
        other.state = TaskState::Archived;
        store
            .save(&mut other, "test_transition", json!({}))
            .unwrap();
        repair.state = TaskState::Completed;
        store
            .save(&mut repair, "test_transition", json!({}))
            .unwrap();
        resolve_repairs(store, &repair).unwrap();
        resolve_repairs(store, &repair).unwrap(); // Durable replay is idempotent.
        let restored = store.get("original").unwrap();
        assert_eq!(restored.state, TaskState::NeedsAttention); // Repair chain restored main, not the original goal.
        assert_eq!(restored.repaired_by.as_deref(), Some("repair"));
        assert_eq!(restored.merge_sha, original.merge_sha);
        assert_eq!(
            serde_json::to_value(&restored.post_ci).unwrap(),
            serde_json::to_value(&original.post_ci).unwrap()
        );
        assert_eq!(
            store.get("failed-repair").unwrap().state,
            TaskState::Completed
        );
        assert!(
            merge_blocker(store, &task("next", TaskState::ReadyToMerge, None, false))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn broken_repair_lineage_fails_closed() {
        let harness = TestStore::new();
        let store = harness.get();
        let a = task("a", TaskState::IntegrationBlocked, Some("b"), false);
        let b = task("b", TaskState::IntegrationBlocked, Some("a"), false);
        store.insert(&a).unwrap();
        store.insert(&b).unwrap();
        assert!(
            merge_blocker(
                store,
                &task("repair", TaskState::ReadyToMerge, Some("a"), true)
            )
            .is_err()
        );
        assert!(
            merge_blocker(
                store,
                &task("missing", TaskState::ReadyToMerge, Some("absent"), true)
            )
            .is_err()
        );
    }

    #[test]
    fn test_merge_ci_evidence_is_bound_to_reviewed_base_and_head() {
        let harness = TestStore::new();
        let store = harness.get();
        let mut item = task("ci-source", TaskState::ReadyToMerge, None, false);
        store.insert(&item).unwrap();
        let base = "a".repeat(40);
        let head = "b".repeat(40);
        let test_merge = "d".repeat(40);
        store
            .save(
                &mut item,
                "pr_ci_passed",
                json!({
                    "base_sha":base, "head_sha":head, "evidence":{"head_sha":test_merge}
                }),
            )
            .unwrap();
        assert_eq!(
            recorded_ci_commit(store, &item, &base, &head).unwrap(),
            Some(test_merge)
        );
        assert_eq!(
            recorded_ci_commit(store, &item, &"e".repeat(40), &head).unwrap(),
            None
        );
        assert_eq!(
            recorded_ci_commit(store, &item, &base, &"f".repeat(40)).unwrap(),
            None
        );
    }
}
