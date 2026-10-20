use std::{
    fs::{self, OpenOptions},
    io,
    os::unix::fs::{FileTypeExt, PermissionsExt},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    time::{Duration, timeout},
};

use crate::{
    command, config, delivery,
    model::{Config, Request, Response, Task, TaskState, now},
    runtime,
    store::Store,
};

const REQUEST_LIMIT: u64 = 1024 * 1024;

pub async fn run(config: Config) -> Result<()> {
    for path in [&config.state_dir, &config.worktree_root] {
        fs::create_dir_all(path)?;
    }
    fs::set_permissions(&config.state_dir, fs::Permissions::from_mode(0o700))?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(config.state_dir.join("daemon.lock"))?;
    lock.try_lock_exclusive()
        .context("another controller already owns this state directory")?;
    let root = command::run("git", &["rev-parse", "--show-toplevel"], &config.repository).await?;
    ensure!(
        fs::canonicalize(root.trim())? == config.repository,
        "repository must name the Git working tree root"
    );
    command::run(
        "git",
        &["check-ref-format", "--branch", &config.default_branch],
        &config.repository,
    )
    .await?;
    if let Some(github) = &config.github {
        ensure!(
            std::env::var("GH_HOST").unwrap_or_else(|_| "github.com".into()) == "github.com",
            "this configuration targets github.com; unset a different GH_HOST"
        );
        let remote = command::run(
            "git",
            &["remote", "get-url", &config.remote],
            &config.repository,
        )
        .await?;
        let remote = remote.trim().trim_end_matches('/').trim_end_matches(".git");
        let repository = [
            "https://github.com/",
            "git@github.com:",
            "ssh://git@github.com/",
        ]
        .iter()
        .find_map(|prefix| remote.strip_prefix(prefix));
        ensure!(
            repository.is_some_and(|name| name.eq_ignore_ascii_case(&github.repository)),
            "Git remote must point directly to the configured github.com OWNER/REPOSITORY"
        );
        command::run(&config.gh, &["--version"], &config.repository).await?;
    }
    command::run(&config.tmux, &["-V"], &config.repository)
        .await
        .context("install tmux or configure its executable path")?;
    command::run(&config.omp, &["--version"], &config.repository)
        .await
        .context("omp executable is unavailable")?;
    let store = Store::open(&config.state_dir)?;
    let socket = config.socket();
    if socket.exists() {
        ensure!(
            fs::symlink_metadata(&socket)?.file_type().is_socket(),
            "refusing to replace non-socket {}",
            socket.display()
        );
        ensure!(
            UnixStream::connect(&socket).await.is_err(),
            "a live process already serves this socket"
        );
        fs::remove_file(&socket)?;
    }
    let listener = UnixListener::bind(&socket)?;
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    eprintln!("int-agent controller listening on {}", socket.display());
    let mut clock = tokio::time::interval(Duration::from_secs(config.poll_seconds));
    clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut cursor = 0usize;
    let signal = tokio::signal::ctrl_c();
    tokio::pin!(signal);
    loop {
        tokio::select! {
            _ = &mut signal => break,
            connection = listener.accept() => {
                let (stream, _) = connection?;
                if let Err(error) = handle_stream(stream, &config, &store).await {
                    eprintln!("controller connection: {error:#}");
                }
            }
            _ = clock.tick() => {
                let active = store.list_active()?;
                if !active.is_empty() {
                    cursor %= active.len();
                    let mut task = active.into_iter().nth(cursor).expect("nonempty tasks");
                    cursor += 1;
                    if let Err(error) = advance(&config, &store, &mut task).await {
                        record_error(&store, &mut task, &format!("{error:#}"))?;
                    }
                }
            }
        }
    }
    fs::remove_file(&socket)?;
    eprintln!("controller stopped; independent omp processes and task records are retained");
    Ok(())
}

async fn handle_stream(stream: UnixStream, config: &Config, store: &Store) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut limited = BufReader::new(reader).take(REQUEST_LIMIT + 1);
    let mut input = Vec::new();
    let read = timeout(
        Duration::from_secs(5),
        limited.read_until(b'\n', &mut input),
    )
    .await
    .context("request frame did not arrive within 5 seconds")??;
    let result = if read as u64 > REQUEST_LIMIT {
        Err(anyhow::anyhow!("request exceeds 1 MiB"))
    } else if input.last() != Some(&b'\n') {
        Err(anyhow::anyhow!("request ended without a JSONL newline"))
    } else {
        match serde_json::from_slice::<Request>(&input) {
            Ok(request) => handle(request, config, store).await,
            Err(error) => Err(error.into()),
        }
    };
    let response = match result {
        Ok(result) => Response {
            ok: true,
            result: Some(result),
            error: None,
        },
        Err(error) => Response {
            ok: false,
            result: None,
            error: Some(format!("{error:#}")),
        },
    };
    let mut bytes = serde_json::to_vec(&response)?;
    if bytes.len() >= 8 * 1024 * 1024 {
        bytes = serde_json::to_vec(&Response {
            ok: false, result: None,
            error: Some("response exceeds 8 MiB; reduce query page limits (event_limit=0 omits history). Accepted mutations remain persisted.".into()),
        })?;
    }
    bytes.push(b'\n');
    // The accepted operation has already been persisted even if its MCP client disconnected.
    timeout(Duration::from_secs(5), writer.write_all(&bytes))
        .await
        .context("response reader stalled")??;
    Ok(())
}

async fn handle(request: Request, config: &Config, store: &Store) -> Result<Value> {
    match request {
        Request::Ping {} => Ok(
            json!({"version": env!("CARGO_PKG_VERSION"), "repository": config.repository, "socket": config.socket()}),
        ),
        Request::List { after, limit } => {
            let (tasks, next_after) = store.task_page(after.as_deref(), limit.unwrap_or(25))?;
            let blockers = store
                .list_active()?
                .into_iter()
                .filter(|t| t.state.integration())
                .map(|t| t.id)
                .collect::<Vec<_>>();
            Ok(json!({"tasks":tasks,"next_after":next_after,"integration_blockers":blockers}))
        }
        Request::Get {
            task_id,
            events_after,
            event_limit,
        } => {
            let task = store.get(&task_id)?;
            let (events, events_next_after) = store.event_page(
                &task_id,
                events_after.unwrap_or(0),
                event_limit.unwrap_or(25),
            )?;
            let worker_pending = task
                .worker
                .as_ref()
                .map(|r| read_json(&r.runtime_dir.join("pending.json")))
                .transpose()?
                .flatten();
            let reviewer_pending = task
                .reviewer
                .as_ref()
                .map(|r| read_json(&r.runtime_dir.join("pending.json")))
                .transpose()?
                .flatten();
            let terminal = format!(
                "{} -S {} attach -t int-agent",
                config.tmux,
                config.tmux_socket().display()
            );
            Ok(
                json!({"task":task,"worker_pending":worker_pending,"reviewer_pending":reviewer_pending,
                "events":events,"events_next_after":events_next_after,"terminal_command":terminal}),
            )
        }
        Request::Start { spec } => {
            config::validate_task(config, &spec)?;
            if let Some(task) = store.find_request(&spec.request_id)? {
                ensure!(
                    task.spec == spec,
                    "request_id already belongs to a different task specification"
                );
                return Ok(serde_json::to_value(task)?);
            }
            let existing = store.list_active()?;
            if let Some(parent) = &spec.repairs_task_id {
                ensure!(
                    store.get(parent)?.state == TaskState::IntegrationBlocked,
                    "repairs_task_id must identify an integration-blocked task"
                );
                ensure!(
                    !existing
                        .iter()
                        .any(|t| t.spec.repairs_task_id.as_ref() == Some(parent)
                            && !t.state.terminal()
                            && t.state != TaskState::Completed),
                    "an active repair already exists for this task"
                );
            }
            let mut repair_ancestors = std::collections::HashSet::new();
            let mut ancestor_id = spec.repairs_task_id.clone();
            while let Some(id) = ancestor_id {
                ensure!(repair_ancestors.insert(id.clone()), "cyclic repair lineage");
                ancestor_id = store.get(&id)?.spec.repairs_task_id;
            }
            let active_count = existing
                .iter()
                .filter(|t| {
                    !t.state.terminal()
                        && t.state != TaskState::Completed
                        && !repair_ancestors.contains(&t.id)
                })
                .count();
            ensure!(
                active_count < config.max_workers,
                "worker capacity reached; finish/cancel an active task first"
            );
            let resolved = command::run(
                "git",
                &[
                    "rev-parse",
                    "--verify",
                    &format!("{}^{{commit}}", spec.base_sha),
                ],
                &config.repository,
            )
            .await?;
            ensure!(
                resolved.trim().eq_ignore_ascii_case(&spec.base_sha),
                "base_sha is not the exact commit object"
            );
            let id = format!("t-{}", uuid::Uuid::new_v4().simple());
            let time = now();
            let mut task = Task {
                branch: format!("worker/{id}-{}", spec.branch_slug),
                worktree: config.worktree_root.join(&id),
                id,
                spec,
                state: TaskState::Starting,
                worker: None,
                reviewer: None,
                pr: None,
                review_base: None,
                review_head: None,
                review_report: None,
                merge_authorized_head: None,
                merge_sha: None,
                post_ci: vec![],
                repaired_by: None,
                last_error: None,
                created_at: time.clone(),
                updated_at: time,
            };
            store.insert(&task)?;
            match runtime::start_worker(config, &task, None).await {
                Ok(run) => {
                    task.worker = Some(run);
                    store.save(&mut task, "worker_spawned", json!({}))?;
                }
                Err(error) => {
                    task.state = TaskState::NeedsAttention;
                    record_error(
                        store,
                        &mut task,
                        &format!("worker startup failed: {error:#}"),
                    )?;
                }
            }
            Ok(serde_json::to_value(task)?)
        }
        Request::Cancel { task_id } => {
            let mut task = store.get(&task_id)?;
            ensure!(
                !task.state.integration()
                    && task.state != TaskState::Completed
                    && task.state != TaskState::Archived,
                "cannot cancel merged/integration tasks; repair or verify them instead"
            );
            if task.state != TaskState::Cancelled {
                if !store.cancellation_pending(&task.id)? {
                    task.state = TaskState::NeedsAttention;
                    task.last_error =
                        Some("cancellation requested; stopping owned processes".into());
                    store.save(&mut task, "cancellation_requested", json!({}))?;
                }
                runtime::cancel_task(config, &task).await?;
                task.state = TaskState::Cancelled;
                store.save(&mut task, "cancelled", json!({"resources_retained":true}))?;
            }
            Ok(serde_json::to_value(task)?)
        }
        Request::Resume { task_id } => {
            let mut task = store.get(&task_id)?;
            ensure!(
                !matches!(
                    task.state,
                    TaskState::Cancelled | TaskState::Archived | TaskState::Completed
                ),
                "task is already closed"
            );
            ensure!(
                !store.cancellation_pending(&task.id)?,
                "cancellation is pending; retry cancellation instead"
            );
            if task.state == TaskState::Review {
                let base = task
                    .review_base
                    .as_deref()
                    .context("review has no fixed base")?;
                let head = task
                    .review_head
                    .as_deref()
                    .context("review has no fixed head")?;
                let run = runtime::start_reviewer(config, &task, base, head).await?;
                task.reviewer = Some(run);
                task.last_error = None;
                store.save(&mut task, "reviewer_resumed", json!({}))?;
                return Ok(serde_json::to_value(task)?);
            }
            let run = runtime::start_worker(config, &task, task.worker.as_ref()).await?;
            task.worker = Some(run);
            if matches!(
                task.state,
                TaskState::Failed | TaskState::NeedsAttention | TaskState::Starting
            ) {
                task.state = TaskState::Starting;
            }
            task.last_error = None;
            store.save(&mut task, "worker_resumed", json!({}))?;
            Ok(serde_json::to_value(task)?)
        }
        Request::AuthorizeMerge {
            task_id,
            expected_head,
        } => {
            let mut task = store.get(&task_id)?;
            ensure!(
                task.state == TaskState::ReadyToMerge,
                "task is not ready to merge"
            );
            ensure!(
                task.review_head.as_deref() == Some(&expected_head),
                "authorization head differs from reviewed head"
            );
            task.merge_authorized_head = Some(expected_head);
            store.save(&mut task, "merge_authorized", json!({}))?;
            Ok(serde_json::to_value(task)?)
        }
        Request::Archive { task_id } => {
            let mut task = store.get(&task_id)?;
            ensure!(
                task.state == TaskState::Completed || task.state == TaskState::Archived,
                "only verified completed tasks can be archived"
            );
            if task.state != TaskState::Archived {
                delivery::advance(config, store, &mut task).await?;
            }
            Ok(serde_json::to_value(task)?)
        }
    }
}

async fn advance(config: &Config, store: &Store, task: &mut Task) -> Result<()> {
    if store.cancellation_pending(&task.id)? {
        runtime::cancel_task(config, task).await?;
        task.state = TaskState::Cancelled;
        task.last_error = None;
        store.save(task, "cancelled", json!({"resources_retained":true}))?;
        return Ok(());
    }
    if task.state == TaskState::Starting && task.worker.is_none() {
        // Recover a durable startup intent after a daemon crash; runtime reattaches rather than duplicates.
        match runtime::start_worker(config, task, None).await {
            Ok(run) => {
                task.worker = Some(run);
                store.save(task, "startup_recovered", json!({}))?;
            }
            Err(error) => {
                task.state = TaskState::NeedsAttention;
                return Err(error);
            }
        }
    }
    let old_worker = serde_json::to_value(&task.worker)?;
    let old_reviewer = serde_json::to_value(&task.reviewer)?;
    let mut worker_status = None;
    if let Some(run) = &mut task.worker {
        worker_status = Some(runtime::refresh(config, run).await?);
    }
    if let Some(run) = &mut task.reviewer {
        runtime::refresh(config, run).await?;
    }
    if old_worker != serde_json::to_value(&task.worker)?
        || old_reviewer != serde_json::to_value(&task.reviewer)?
    {
        store.save(task, "session_metadata", json!({}))?;
    }
    if let Some(status) = worker_status
        && matches!(
            task.state,
            TaskState::Starting | TaskState::Running | TaskState::WaitingUser
        )
    {
        let state = if !status.alive {
            TaskState::NeedsAttention
        } else if !status.ready {
            TaskState::Starting
        } else if status.pending.is_some() {
            TaskState::WaitingUser
        } else {
            TaskState::Running
        };
        if state != task.state {
            task.state = state;
            store.save(
                task,
                "worker_state",
                json!({"ready":status.ready,"alive":status.alive}),
            )?;
        }
    }
    delivery::advance(config, store, task).await
}

fn record_error(store: &Store, task: &mut Task, message: &str) -> Result<()> {
    if task.last_error.as_deref() != Some(message) {
        task.last_error = Some(message.to_owned());
        store.save(task, "attention_required", json!({"error":message}))?;
        eprintln!("{}: {}", task.id, message);
    }
    Ok(())
}
fn read_json(path: &Path) -> Result<Option<Value>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
            format!("invalid runtime record {}", path.display())
        })?)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("read {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unterminated_mutation_does_not_cancel_a_task() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let config: Config = serde_json::from_value(json!({
            "repository": directory.path().join("repo"),
            "worktree_root": directory.path().join("worktrees"),
            "state_dir": directory.path()
        }))?;
        let store = Store::open(directory.path())?;
        let task: Task = serde_json::from_value(json!({
            "id":"framing-task", "state":"starting",
            "spec":{"request_id":"framing-task","title":"Keep running","goal":"Keep running",
                "acceptance":["A truncated request must not cancel work"],"base_sha":"a".repeat(40),"branch_slug":"framing"},
            "branch":"worker/framing-task-framing",
            "worktree":config.worktree_root.join("framing-task"),
            "post_ci":[], "created_at":now(), "updated_at":now()
        }))?;
        store.insert(&task)?;
        let (server, mut client) = UnixStream::pair()?;
        let peer = tokio::spawn(async move {
            client
                .write_all(br#"{"op":"cancel","task_id":"framing-task"}"#)
                .await?;
            client.shutdown().await?;
            let mut response = Vec::new();
            client.read_to_end(&mut response).await?;
            Ok::<_, anyhow::Error>(serde_json::from_slice::<Response>(&response)?)
        });
        handle_stream(server, &config, &store).await?;
        let response = peer.await??;
        assert!(!response.ok);
        assert_eq!(store.get(&task.id)?.state, TaskState::Starting);
        assert!(!store.cancellation_pending(&task.id)?);
        Ok(())
    }
}
