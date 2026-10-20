use std::{
    fs,
    path::{Component, Path},
};

use crate::model::Config;
use anyhow::{Context, Result, bail, ensure};

pub fn load(path: &Path) -> Result<Config> {
    let bytes = fs::read(path).with_context(|| format!("read configuration {}", path.display()))?;
    let mut config: Config =
        serde_json::from_slice(&bytes).context("invalid configuration JSON")?;
    for (name, value) in [
        ("repository", &config.repository),
        ("worktree_root", &config.worktree_root),
        ("state_dir", &config.state_dir),
    ] {
        ensure!(value.is_absolute(), "{name} must be an absolute path");
        ensure!(
            !value
                .components()
                .any(|c| matches!(c, Component::ParentDir)),
            "{name} must not contain '..'"
        );
        ensure!(
            value.parent().is_some(),
            "{name} must not be the filesystem root"
        );
    }
    config.repository =
        fs::canonicalize(&config.repository).context("repository directory does not exist")?;
    ensure!(config.repository.is_dir(), "repository must be a directory");
    // Resolve existing ancestors too: a nonexistent child below a symlink must not evade overlap checks.
    config.worktree_root = resolve_future(&config.worktree_root)?;
    config.state_dir = resolve_future(&config.state_dir)?;
    for (a, b) in [
        (&config.repository, &config.worktree_root),
        (&config.repository, &config.state_dir),
        (&config.worktree_root, &config.state_dir),
    ] {
        ensure!(
            !a.starts_with(b) && !b.starts_with(a),
            "repository, worktree_root and state_dir must be disjoint directories"
        );
    }
    ensure!(
        config.socket().as_os_str().as_encoded_bytes().len() < 104,
        "state_dir is too long for a Unix socket path"
    );
    ensure!(
        (1..=60).contains(&config.poll_seconds),
        "poll_seconds must be 1..=60"
    );
    ensure!(
        (1..=32).contains(&config.max_workers),
        "max_workers must be 1..=32"
    );
    ensure!(
        ["write", "always-ask", "yolo"].contains(&config.approval_mode.as_str()),
        "invalid approval_mode"
    );
    ensure!(
        safe_name(&config.remote),
        "remote must be a simple Git remote name"
    );
    ensure!(
        !config.default_branch.is_empty()
            && !config.default_branch.starts_with('-')
            && !config.default_branch.chars().any(char::is_control),
        "invalid default_branch"
    );
    for command in [&config.omp, &config.tmux, &config.gh] {
        ensure!(
            !command.is_empty() && !command.chars().any(char::is_control),
            "invalid executable name"
        );
    }
    if let Some(github) = &config.github {
        let parts: Vec<_> = github.repository.split('/').collect();
        ensure!(
            parts.len() == 2 && parts.iter().all(|p| safe_name(p)),
            "github.repository must be OWNER/REPOSITORY"
        );
        ensure!(
            !github.required_checks.is_empty(),
            "github.required_checks must name real required jobs"
        );
        ensure!(
            !github.post_merge_workflows.is_empty(),
            "github.post_merge_workflows must name real workflow files"
        );
        ensure!(
            github.required_checks.iter().all(|s| !s.trim().is_empty()),
            "required check names must not be empty"
        );
        ensure!(
            github
                .post_merge_workflows
                .iter()
                .all(|s| !s.trim().is_empty()),
            "workflow names must not be empty"
        );
    }
    Ok(config)
}

fn resolve_future(path: &Path) -> Result<std::path::PathBuf> {
    let mut ancestor = path;
    let mut missing = Vec::new();
    while !ancestor.exists() {
        missing.push(
            ancestor
                .file_name()
                .context("path has no existing ancestor")?
                .to_owned(),
        );
        ancestor = ancestor.parent().context("path has no parent")?;
    }
    let mut resolved = fs::canonicalize(ancestor)?;
    for component in missing.into_iter().rev() {
        resolved.push(component);
    }
    Ok(resolved)
}

pub fn safe_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 100
        && !value.starts_with('-')
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

pub fn validate_task(config: &Config, spec: &crate::model::StartTask) -> Result<()> {
    ensure!(
        safe_name(&spec.request_id),
        "request_id must be 1..=100 simple ASCII characters"
    );
    ensure!(
        safe_name(&spec.branch_slug) && !spec.branch_slug.contains('.'),
        "branch_slug must contain only letters, digits, '-' or '_'"
    );
    ensure!(
        !spec.title.trim().is_empty() && spec.title.len() <= 300,
        "title must be nonempty and at most 300 bytes"
    );
    ensure!(
        !spec.goal.trim().is_empty() && spec.goal.len() <= 100_000,
        "goal must be nonempty and at most 100000 bytes"
    );
    ensure!(
        !spec.acceptance.is_empty() && spec.acceptance.iter().all(|s| !s.trim().is_empty()),
        "acceptance must contain concrete nonempty criteria"
    );
    ensure!(
        [40, 64].contains(&spec.base_sha.len())
            && spec.base_sha.bytes().all(|b| b.is_ascii_hexdigit()),
        "base_sha must be a full commit SHA"
    );
    if let Some(model) = &spec.model
        && !config.allowed_models.contains(model)
        && config.worker_model.as_ref() != Some(model)
    {
        bail!("model override is not in allowed_models or worker_model");
    }
    if let Some(id) = &spec.repairs_task_id {
        ensure!(safe_name(id), "invalid repairs_task_id");
    }
    ensure!(
        !spec.completes_original_goal || spec.repairs_task_id.is_some(),
        "completes_original_goal requires repairs_task_id"
    );
    Ok(())
}
