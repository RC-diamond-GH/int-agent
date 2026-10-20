use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{
    command,
    model::{AgentRun, Config, Task, now},
};

const SESSION: &str = "int-agent";
const OWNER_FILE: &str = "int-agent-owner.json";

#[derive(Debug)]
pub struct RunStatus {
    pub alive: bool,
    pub ready: bool,
    pub pending: Option<Value>,
}

#[derive(Clone, Serialize, Deserialize)]
struct Ownership {
    task_id: String,
    repository: PathBuf,
    worktree: PathBuf,
    branch: Option<String>,
    base: String,
    token: String,
    removed: bool,
}

#[derive(Clone, Serialize, Deserialize)]
struct Launch {
    task_id: String,
    run: AgentRun,
    index: u32,
    base: Option<String>,
    head: Option<String>,
    cwd: PathBuf,
    resume_file: Option<PathBuf>,
    phase: String,
}

#[derive(Deserialize)]
struct Observer {
    run_id: String,
    pid: u32,
    #[serde(rename = "sessionFile")]
    session_file: Option<PathBuf>,
    cwd: PathBuf,
    ready: bool,
}

fn text(path: &Path) -> Result<&str> {
    path.to_str().context("runtime path is not UTF-8")
}
fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
}
fn sha(value: &str) -> Result<()> {
    ensure!(
        (value.len() == 40 || value.len() == 64) && value.bytes().all(|c| c.is_ascii_hexdigit()),
        "expected a full git commit SHA"
    );
    Ok(())
}
fn atomic(path: &Path, data: &impl Serialize) -> Result<()> {
    let parent = path.parent().context("file has no parent")?;
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".{}.tmp", Uuid::new_v4()));
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&serde_json::to_vec_pretty(data)?)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let metadata = fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    ensure!(
        metadata.len() <= 1024 * 1024,
        "runtime metadata exceeds 1 MiB"
    );
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
fn task_root(config: &Config, task: &Task) -> Result<PathBuf> {
    ensure!(valid_id(&task.id), "invalid task identity");
    ensure!(valid_id(&task.spec.branch_slug), "invalid task branch slug");
    ensure!(
        task.branch == format!("worker/{}-{}", task.id, task.spec.branch_slug),
        "task branch does not match its identity"
    );
    ensure!(
        task.worktree.parent() == Some(config.worktree_root.as_path()),
        "task worktree is not a direct child of the configured root"
    );
    ensure!(task.worktree.file_name().is_some(), "missing worktree name");
    Ok(config.task_dir(&task.id))
}
fn launch_path(run: &AgentRun) -> PathBuf {
    run.runtime_dir.join("launch.json")
}
fn owner_path(config: &Config, task: &Task) -> PathBuf {
    config.task_dir(&task.id).join("worker-worktree.json")
}
fn validate_run(config: &Config, run: &AgentRun) -> Result<Launch> {
    Uuid::parse_str(&run.id).context("invalid run identity")?;
    let launch: Launch = read_json(&launch_path(run))?;
    ensure!(valid_id(&launch.task_id), "invalid task identity in launch");
    let expected = config.task_dir(&launch.task_id).join("runs").join(&run.id);
    ensure!(
        run.runtime_dir == expected && launch.run.runtime_dir == expected,
        "run directory is not controller owned"
    );
    ensure!(
        launch.run.id == run.id
            && launch.run.role == run.role
            && launch.run.worktree == run.worktree,
        "run identity disagrees with launch manifest"
    );
    ensure!(
        run.role == "worker" || run.role == "reviewer",
        "unknown runtime role"
    );
    Ok(launch)
}
async fn git(config: &Config, args: &[&str]) -> Result<String> {
    command::run("git", args, &config.repository).await
}
async fn tmux(config: &Config, args: &[&str]) -> Result<String> {
    let socket = config.tmux_socket();
    let mut all = vec!["-S", text(&socket)?];
    all.extend_from_slice(args);
    command::run(&config.tmux, &all, &config.repository).await
}
async fn tmux_output(config: &Config, args: &[&str]) -> Result<command::Output> {
    let socket = config.tmux_socket();
    let mut all = vec!["-S", text(&socket)?];
    all.extend_from_slice(args);
    command::output(&config.tmux, &all, &config.repository).await
}
async fn ensure_server(config: &Config) -> Result<()> {
    let status = tmux_output(config, &["has-session", "-t", SESSION]).await?;
    if status.code != 0 {
        tmux(
            config,
            &[
                "new-session",
                "-d",
                "-s",
                SESSION,
                "-n",
                "ops",
                "-c",
                text(&config.repository)?,
            ],
        )
        .await?;
    }
    Ok(())
}

// Read the environment of the pane's actual process, not a mutable window name.
// A private tmux socket alone does not prove ownership after server/pane reuse.
fn process_owned(pid: u32, id: &str) -> bool {
    fs::read(format!("/proc/{pid}/environ"))
        .map(|bytes| {
            let expected = format!("INT_AGENT_RUN_ID={id}");
            bytes
                .split(|c| *c == 0)
                .any(|entry| entry == expected.as_bytes())
        })
        .unwrap_or(false)
}
async fn find_pane(config: &Config, id: &str) -> Result<Option<(String, u32)>> {
    let result = tmux_output(
        config,
        &[
            "list-panes",
            "-a",
            "-F",
            "#{pane_id}\t#{pane_pid}\t#{pane_dead}",
        ],
    )
    .await?;
    if result.code != 0 {
        if result.stderr.contains("no server running")
            || result.stderr.contains("error connecting")
            || result.stderr.contains("No such file or directory")
        {
            return Ok(None);
        }
        bail!("cannot inspect tmux: {}", result.stderr);
    }
    let mut found = None;
    for line in result.stdout.lines() {
        let fields: Vec<_> = line.split('\t').collect();
        if fields.len() != 3 || fields[2] != "0" {
            continue;
        }
        let Ok(pid) = fields[1].parse::<u32>() else {
            continue;
        };
        if process_owned(pid, id) {
            ensure!(found.is_none(), "multiple tmux panes claim run {id}");
            ensure!(
                fields[0].starts_with('%') && fields[0][1..].bytes().all(|b| b.is_ascii_digit()),
                "invalid tmux pane id"
            );
            found = Some((fields[0].to_owned(), pid));
        }
    }
    Ok(found)
}
async fn git_dir(worktree: &Path) -> Result<PathBuf> {
    Ok(PathBuf::from(
        command::run("git", &["rev-parse", "--absolute-git-dir"], worktree)
            .await?
            .trim(),
    ))
}
async fn mark_owner(config: &Config, owner: &Ownership) -> Result<()> {
    ensure!(
        fs::symlink_metadata(&owner.worktree)?.file_type().is_dir(),
        "worktree is not a real directory"
    );
    let list = git(config, &["worktree", "list", "--porcelain"]).await?;
    let registered = format!("worktree {}", text(&owner.worktree)?);
    ensure!(
        list.split("\n\n")
            .any(|block| block.lines().next() == Some(registered.as_str())),
        "refusing to mark an unregistered worktree"
    );
    let expected = git(
        config,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    let actual = command::run(
        "git",
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
        &owner.worktree,
    )
    .await?;
    ensure!(
        fs::canonicalize(expected.trim())? == fs::canonicalize(actual.trim())?,
        "worktree belongs to another repository"
    );
    atomic(&git_dir(&owner.worktree).await?.join(OWNER_FILE), owner)
}
async fn verify_owner(config: &Config, owner: &Ownership) -> Result<()> {
    ensure!(
        owner.repository == config.repository && !owner.removed,
        "worktree ownership is no longer active"
    );
    ensure!(
        owner.worktree.parent() == Some(config.worktree_root.as_path()),
        "worktree escapes configured root"
    );
    ensure!(
        fs::symlink_metadata(&owner.worktree)?.file_type().is_dir(),
        "worktree is not an owned real directory"
    );
    let list = git(config, &["worktree", "list", "--porcelain"]).await?;
    let line = format!("worktree {}", text(&owner.worktree)?);
    ensure!(
        list.split("\n\n")
            .any(|block| block.lines().next() == Some(line.as_str())),
        "worktree is not registered in configured repository"
    );
    let marker: Ownership = read_json(&git_dir(&owner.worktree).await?.join(OWNER_FILE))?;
    ensure!(
        marker.token == owner.token
            && marker.task_id == owner.task_id
            && marker.worktree == owner.worktree
            && marker.repository == owner.repository,
        "git worktree ownership marker does not match"
    );
    let actual = command::output(
        "git",
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
        &owner.worktree,
    )
    .await?;
    match &owner.branch {
        Some(branch) => ensure!(
            actual.code == 0 && actual.stdout.trim() == branch,
            "owned worktree switched branches"
        ),
        None => {
            ensure!(actual.code == 1, "reviewer worktree is no longer detached");
            let head = command::run("git", &["rev-parse", "HEAD"], &owner.worktree).await?;
            ensure!(
                head.trim() == owner.base,
                "reviewer worktree changed its commit"
            );
        }
    }
    Ok(())
}
async fn prepare_worker(config: &Config, task: &Task) -> Result<Ownership> {
    let root = task_root(config, task)?;
    fs::create_dir_all(&root)?;
    fs::create_dir_all(&config.worktree_root)?;
    let path = owner_path(config, task);
    let owner = if path.exists() {
        let saved: Ownership = read_json(&path)?;
        ensure!(
            saved.task_id == task.id
                && saved.worktree == task.worktree
                && saved.branch.as_deref() == Some(task.branch.as_str())
                && saved.base == task.spec.base_sha,
            "worker ownership manifest changed"
        );
        ensure!(!saved.removed, "archived worktree cannot be resumed");
        saved
    } else {
        ensure!(
            !task.worktree.exists(),
            "refusing to adopt an existing worktree directory"
        );
        let reference = format!("refs/heads/{}", task.branch);
        let exists = command::output(
            "git",
            &["show-ref", "--verify", "--quiet", &reference],
            &config.repository,
        )
        .await?;
        ensure!(
            exists.code == 1,
            "refusing to adopt existing branch or branch lookup failed"
        );
        sha(&task.spec.base_sha)?;
        let owner = Ownership {
            task_id: task.id.clone(),
            repository: config.repository.clone(),
            worktree: task.worktree.clone(),
            branch: Some(task.branch.clone()),
            base: task.spec.base_sha.clone(),
            token: Uuid::new_v4().to_string(),
            removed: false,
        };
        atomic(&path, &owner)?;
        owner
    };
    if !owner.worktree.exists() {
        let reference = format!("refs/heads/{}", task.branch);
        let exists = command::output(
            "git",
            &["show-ref", "--verify", "--quiet", &reference],
            &config.repository,
        )
        .await?;
        if exists.code == 1 {
            git(
                config,
                &[
                    "worktree",
                    "add",
                    "-b",
                    &task.branch,
                    text(&owner.worktree)?,
                    &owner.base,
                ],
            )
            .await?;
        } else {
            ensure!(exists.code == 0, "cannot query owned branch");
            let tip = git(config, &["rev-parse", "--verify", &reference]).await?;
            ensure!(
                tip.trim() == owner.base,
                "unregistered branch has unexpected commits; refusing adoption"
            );
            git(
                config,
                &["worktree", "add", text(&owner.worktree)?, &task.branch],
            )
            .await?;
        }
        mark_owner(config, &owner).await?;
    } else if !git_dir(&owner.worktree).await?.join(OWNER_FILE).exists() {
        // Recover the narrow crash gap after worktree add and before marker write.
        let head = command::run("git", &["rev-parse", "HEAD"], &owner.worktree).await?;
        let status = command::run(
            "git",
            &["status", "--porcelain", "--untracked-files=all"],
            &owner.worktree,
        )
        .await?;
        ensure!(
            head.trim() == owner.base && status.trim().is_empty(),
            "unmarked worktree contains changes; refusing adoption"
        );
        mark_owner(config, &owner).await?;
    }
    verify_owner(config, &owner).await?;
    Ok(owner)
}
fn launches(config: &Config, task: &Task) -> Result<Vec<Launch>> {
    let directory = task_root(config, task)?.join("runs");
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut runs = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        ensure!(
            entry.file_type()?.is_dir(),
            "unexpected entry in owned runs directory"
        );
        let path = entry.path().join("launch.json");
        if !path.exists() {
            continue;
        }
        let launch: Launch = read_json(&path)?;
        ensure!(
            launch.task_id == task.id,
            "foreign task launch in task directory"
        );
        validate_run(config, &launch.run)?;
        runs.push(launch);
    }
    runs.sort_by(|a, b| a.run.started_at.cmp(&b.run.started_at));
    Ok(runs)
}
fn observer(run: &AgentRun) -> Result<Option<Observer>> {
    let path = run.runtime_dir.join("observer.json");
    if !path.exists() {
        return Ok(None);
    }
    let data: Observer = read_json(&path)?;
    ensure!(data.run_id == run.id, "observer identity mismatch");
    Ok(Some(data))
}

pub async fn refresh(config: &Config, run: &mut AgentRun) -> Result<RunStatus> {
    let mut launch = validate_run(config, run)?;
    let pane = find_pane(config, &run.id).await?;
    let data = observer(run)?;
    let alive = pane.is_some();
    if let Some((pane, _)) = pane {
        run.pane = pane;
    }
    let ready = if let Some(data) = data {
        ensure!(data.cwd.is_absolute(), "observer cwd is not absolute");
        ensure!(
            data.cwd == run.worktree || data.cwd.starts_with(&run.worktree),
            "observer cwd left the owned worktree"
        );
        if let Some(file) = &data.session_file {
            ensure!(
                file.starts_with(&run.session_dir),
                "observer session left its owned session directory"
            );
        }
        run.pid = Some(data.pid);
        run.session_file = data.session_file;
        launch.cwd = data.cwd;
        alive && data.ready && process_owned(data.pid, &run.id)
    } else {
        false
    };
    let pending_path = run.runtime_dir.join("pending.json");
    let pending = if ready && pending_path.exists() {
        let value: Value = read_json(&pending_path)?;
        ensure!(
            value.get("run_id").and_then(Value::as_str) == Some(run.id.as_str()),
            "pending ask identity mismatch"
        );
        Some(value)
    } else {
        None
    };
    launch.run = run.clone();
    if ready {
        launch.phase = "ready".into();
    }
    atomic(&launch_path(run), &launch)?;
    Ok(RunStatus {
        alive,
        ready,
        pending,
    })
}

fn new_launch(
    config: &Config,
    task: &Task,
    role: &str,
    worktree: PathBuf,
    base: Option<String>,
    head: Option<String>,
    previous: Option<&Launch>,
) -> Result<Launch> {
    let id = Uuid::new_v4();
    let runtime_dir = config.task_dir(&task.id).join("runs").join(id.to_string());
    fs::create_dir_all(&runtime_dir)?;
    let session_dir = config.task_dir(&task.id).join("sessions").join(role);
    fs::create_dir_all(&session_dir)?;
    let resume_file = previous
        .and_then(|p| p.run.session_file.clone())
        .filter(|p| p.is_file());
    let cwd = previous
        .map(|p| p.cwd.clone())
        .unwrap_or_else(|| worktree.clone());
    ensure!(
        cwd.is_dir() && (cwd == worktree || cwd.starts_with(&worktree)),
        "saved session cwd is missing or outside its worktree"
    );
    let window = if role == "worker" {
        format!("worker-{}", task.id)
    } else {
        format!(
            "reviewer-{}-{}",
            task.id,
            &head.as_deref().context("review head missing")?[..8]
        )
    };
    let run = AgentRun {
        id: id.to_string(),
        role: role.into(),
        worktree,
        pane: String::new(),
        window,
        runtime_dir,
        session_dir,
        session_file: None,
        pid: None,
        started_at: now(),
    };
    let index = (id.as_u128() as u32 & 0x3fff_ffff) + 1;
    let launch = Launch {
        task_id: task.id.clone(),
        run,
        index,
        base,
        head,
        cwd,
        resume_file,
        phase: "intent".into(),
    };
    atomic(&launch_path(&launch.run), &launch)?;
    Ok(launch)
}

async fn launch(config: &Config, task: &Task, mut intent: Launch) -> Result<AgentRun> {
    let run = &intent.run;
    let owned_path = if run.role == "worker" {
        owner_path(config, task)
    } else {
        config.task_dir(&task.id).join(format!(
            "reviewer-owner-{}-{}.json",
            intent.base.as_deref().context("missing reviewer base")?,
            intent.head.as_deref().context("missing reviewer head")?
        ))
    };
    let owned: Ownership = read_json(&owned_path)?;
    ensure!(
        owned.task_id == task.id && owned.worktree == run.worktree,
        "launch worktree ownership mismatch"
    );
    verify_owner(config, &owned).await?;
    if run.role == "reviewer" {
        atomic(&run.runtime_dir.join("worktree-owner.json"), &owned)?;
    }
    fs::write(
        run.runtime_dir.join("observer.ts"),
        include_str!("../assets/observer.ts"),
    )?;
    let prompt = if run.role == "worker" {
        include_str!("../assets/worker.md")
    } else {
        include_str!("../assets/reviewer.md")
    };
    fs::write(run.runtime_dir.join("role.md"), prompt)?;
    atomic(
        &run.runtime_dir.join("omp-config.json"),
        &json!({"ask":{"enabled":true,"timeout":0},"autoResume":false}),
    )?;
    let inbox = if run.role == "worker" {
        config.task_dir(&task.id).join("inbox").join("worker")
    } else {
        run.runtime_dir.join("inbox")
    };
    fs::create_dir_all(&inbox)?;
    let task_prompt = if run.role == "worker" {
        format!(
            "Assigned task specification:\n{}\nOwned branch: {}\nIntegration branch: {}\nGitHub delivery configured: {}. Work in {}.",
            serde_json::to_string_pretty(&task.spec)?,
            task.branch,
            config.default_branch,
            config.github.is_some(),
            run.worktree.display()
        )
    } else {
        format!(
            "Review task specification:\n{}\nExact base SHA: {}\nExact head SHA: {}\nRequired report path: {}\nReview git diff BASE...HEAD and acceptance criteria. Write the strict JSON report atomically, then stay idle.",
            serde_json::to_string_pretty(&task.spec)?,
            intent.base.as_deref().context("missing review base")?,
            intent.head.as_deref().context("missing review head")?,
            report_path(run).display()
        )
    };
    let assignment = run.runtime_dir.join("assignment.txt");
    fs::write(&assignment, &task_prompt)?;
    ensure_server(config).await?;
    let target = format!("{SESSION}:{}", intent.index);
    let socket = config.tmux_socket();
    let mut args = vec![
        "new-window".to_owned(),
        "-d".into(),
        "-P".into(),
        "-F".into(),
        "#{pane_id}".into(),
        "-t".into(),
        target,
        "-n".into(),
        run.window.clone(),
        "-c".into(),
        text(&intent.cwd)?.into(),
    ];
    for (key, value) in [
        ("INT_AGENT_RUN_ID", run.id.clone()),
        ("INT_AGENT_RUNTIME_DIR", text(&run.runtime_dir)?.into()),
        ("INT_AGENT_INBOX", text(&inbox)?.into()),
        ("INT_AGENT_TMUX", config.tmux.clone()),
        ("INT_AGENT_TMUX_SOCKET", text(&socket)?.into()),
        ("INT_AGENT_WINDOW", run.window.clone()),
        ("PI_CONFIG_FILES", String::new()),
    ] {
        args.extend(["-e".into(), format!("{key}={value}")]);
    }
    // Multiple shell-command argv makes tmux exec directly, without interpreting task text.
    args.extend([
        config.omp.clone(),
        "--no-extensions".into(),
        "--extension".into(),
        text(&run.runtime_dir.join("observer.ts"))?.into(),
        "--config".into(),
        text(&run.runtime_dir.join("omp-config.json"))?.into(),
        "--session-dir".into(),
        text(&run.session_dir)?.into(),
        "--approval-mode".into(),
        config.approval_mode.clone(),
        "--append-system-prompt".into(),
        prompt.to_owned(),
    ]);
    let model = if run.role == "worker" {
        task.spec.model.as_ref().or(config.worker_model.as_ref())
    } else {
        config.reviewer_model.as_ref()
    };
    if let Some(model) = model {
        args.extend(["--model".into(), model.clone()]);
    }
    if let Some(session) = &intent.resume_file {
        args.extend(["--session".into(), text(session)?.into()]);
    }
    let initial = if intent.resume_file.is_some() {
        "Resume this assigned task in the saved session. Preserve prior user decisions; native questions still require the user. The attached current assignment and report destination supersede any old launch paths."
    } else {
        "Carry out the attached task assignment. Follow the role instructions and exact owned worktree/report destination."
    };
    // Native @file input avoids Linux's per-argument size limit for large task specifications.
    args.extend([
        format!("@{}", text(&assignment)?),
        "--".into(),
        initial.into(),
    ]);
    intent.phase = "spawning".into();
    atomic(&launch_path(&intent.run), &intent)?;
    let refs: Vec<_> = args.iter().map(String::as_str).collect();
    let launched = tmux(config, &refs).await;
    // Even when the client failed, a server may already have accepted the durable launch.
    if let Some((pane, _)) = find_pane(config, &intent.run.id).await? {
        intent.run.pane = pane.clone();
        tmux(
            config,
            &[
                "set-option",
                "-p",
                "-t",
                &pane,
                "@int_agent_run",
                &intent.run.id,
            ],
        )
        .await?;
        tmux(
            config,
            &["set-option", "-w", "-t", &pane, "automatic-rename", "off"],
        )
        .await?;
        tmux(
            config,
            &[
                "set-window-option",
                "-t",
                &pane,
                "window-status-format",
                "#I:#W#{?@int_agent_waiting, [ASK],}",
            ],
        )
        .await?;
        tmux(
            config,
            &[
                "set-window-option",
                "-t",
                &pane,
                "window-status-current-format",
                "#I:#W#{?@int_agent_waiting, [ASK],}*",
            ],
        )
        .await?;
    } else {
        launched.context("tmux did not create the owned worker process")?;
    }
    atomic(&launch_path(&intent.run), &intent)?;
    wait_ready(config, intent.run).await
}
async fn wait_ready(config: &Config, mut run: AgentRun) -> Result<AgentRun> {
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_secs(45);
    let mut tagged = false;
    loop {
        let status = refresh(config, &mut run).await?;
        if status.alive && !tagged {
            tmux(
                config,
                &[
                    "set-option",
                    "-p",
                    "-t",
                    &run.pane,
                    "@int_agent_run",
                    &run.id,
                ],
            )
            .await?;
            tmux(
                config,
                &[
                    "set-option",
                    "-w",
                    "-t",
                    &run.pane,
                    "automatic-rename",
                    "off",
                ],
            )
            .await?;
            tmux(
                config,
                &[
                    "set-window-option",
                    "-t",
                    &run.pane,
                    "window-status-format",
                    "#I:#W#{?@int_agent_waiting, [ASK],}",
                ],
            )
            .await?;
            tmux(
                config,
                &[
                    "set-window-option",
                    "-t",
                    &run.pane,
                    "window-status-current-format",
                    "#I:#W#{?@int_agent_waiting, [ASK],}*",
                ],
            )
            .await?;
            tagged = true;
        }
        if status.ready {
            return Ok(run);
        }
        ensure!(
            status.alive || started.elapsed() < Duration::from_secs(2),
            "{} process exited before observer readiness; launch evidence: {}",
            run.role,
            run.runtime_dir.display()
        );
        ensure!(
            tokio::time::Instant::now() < deadline,
            "{} has not reached observer readiness; its owned pane remains available and will be recovered",
            run.role
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

pub async fn start_worker(
    config: &Config,
    task: &Task,
    resume: Option<&AgentRun>,
) -> Result<AgentRun> {
    prepare_worker(config, task).await?;
    let runs = launches(config, task)?;
    let mut previous = None;
    for saved in runs.iter().filter(|l| l.run.role == "worker").rev() {
        let mut run = saved.run.clone();
        let status = refresh(config, &mut run).await?;
        if status.alive {
            return wait_ready(config, run).await;
        }
        if previous.is_none() {
            let mut saved = saved.clone();
            saved.run = run;
            if let Some(data) = observer(&saved.run)? {
                saved.cwd = data.cwd;
            }
            previous = Some(saved);
        }
    }
    if let Some(resume) = resume {
        let saved = validate_run(config, resume)?;
        ensure!(
            saved.task_id == task.id && saved.run.role == "worker",
            "resume run is not this task's Worker"
        );
        if previous.is_none() {
            previous = Some(saved);
        }
    }
    // A never-spawned intent is reused, including the reserved tmux window index.
    if let Some(saved) = previous
        .as_ref()
        .filter(|p| p.phase == "intent" || p.phase == "spawning")
        && observer(&saved.run)?.is_none()
    {
        return launch(config, task, saved.clone()).await;
    }
    let intent = new_launch(
        config,
        task,
        "worker",
        task.worktree.clone(),
        None,
        None,
        previous.as_ref(),
    )?;
    launch(config, task, intent).await
}

pub async fn start_reviewer(
    config: &Config,
    task: &Task,
    base: &str,
    head: &str,
) -> Result<AgentRun> {
    let root = task_root(config, task)?;
    sha(base)?;
    sha(head)?;
    for commit in [base, head] {
        let object = format!("{commit}^{{commit}}");
        git(config, &["cat-file", "-e", &object]).await
            .with_context(|| format!("review commit {commit} is unavailable locally; fetch the exact PR and base refs first"))?;
    }
    let runs = launches(config, task)?;
    let mut previous = None;
    for saved in runs
        .iter()
        .filter(|l| {
            l.run.role == "reviewer"
                && l.base.as_deref() == Some(base)
                && l.head.as_deref() == Some(head)
        })
        .rev()
    {
        let mut run = saved.run.clone();
        let status = refresh(config, &mut run).await?;
        if status.alive {
            let owner: Ownership = read_json(&run.runtime_dir.join("worktree-owner.json"))?;
            verify_owner(config, &owner).await?;
            return wait_ready(config, run).await;
        }
        if previous.is_none() {
            let mut saved = saved.clone();
            saved.run = run;
            if let Some(data) = observer(&saved.run)? {
                saved.cwd = data.cwd;
            }
            previous = Some(saved);
        }
    }
    if let Some(saved) = previous
        .as_ref()
        .filter(|p| p.phase == "intent" || p.phase == "spawning")
        && observer(&saved.run)?.is_none()
    {
        return launch(config, task, saved.clone()).await;
    }
    fs::create_dir_all(&root)?;
    fs::create_dir_all(&config.worktree_root)?;
    let owner_file = root.join(format!("reviewer-owner-{base}-{head}.json"));
    let existing_owner = if owner_file.exists() {
        Some(read_json::<Ownership>(&owner_file)?)
    } else {
        None
    };
    let worktree = existing_owner
        .as_ref()
        .map(|o| o.worktree.clone())
        .or_else(|| previous.as_ref().map(|p| p.run.worktree.clone()))
        .unwrap_or_else(|| {
            config.worktree_root.join(format!(
                "reviewer-{}-{}-{}",
                task.id,
                &head[..8],
                Uuid::new_v4()
            ))
        });
    let owner = if owner_file.exists() {
        let owner: Ownership = read_json(&owner_file)?;
        ensure!(
            owner.task_id == task.id
                && owner.base == head
                && owner.worktree == worktree
                && owner.branch.is_none(),
            "reviewer ownership mismatch"
        );
        owner
    } else {
        ensure!(
            !worktree.exists(),
            "refusing to adopt existing reviewer worktree"
        );
        let owner = Ownership {
            task_id: task.id.clone(),
            repository: config.repository.clone(),
            worktree: worktree.clone(),
            branch: None,
            base: head.into(),
            token: Uuid::new_v4().to_string(),
            removed: false,
        };
        atomic(&owner_file, &owner)?;
        owner
    };
    if !worktree.exists() {
        git(
            config,
            &["worktree", "add", "--detach", text(&worktree)?, head],
        )
        .await?;
        mark_owner(config, &owner).await?;
    } else if !git_dir(&worktree).await?.join(OWNER_FILE).exists() {
        let actual = command::run("git", &["rev-parse", "HEAD"], &worktree).await?;
        let status = command::run(
            "git",
            &["status", "--porcelain", "--untracked-files=all"],
            &worktree,
        )
        .await?;
        ensure!(
            actual.trim() == head && status.trim().is_empty(),
            "unmarked reviewer tree changed; refusing adoption"
        );
        mark_owner(config, &owner).await?;
    }
    verify_owner(config, &owner).await?;
    let intent = new_launch(
        config,
        task,
        "reviewer",
        worktree,
        Some(base.into()),
        Some(head.into()),
        previous.as_ref(),
    )?;
    atomic(&intent.run.runtime_dir.join("worktree-owner.json"), &owner)?;
    launch(config, task, intent).await
}

pub async fn send_message(config: &Config, run: &AgentRun, message: &str) -> Result<()> {
    let launch = validate_run(config, run)?;
    ensure!(
        message.len() <= 512 * 1024,
        "controller feedback exceeds 512 KiB"
    );
    let inbox = if run.role == "worker" {
        config
            .task_dir(&launch.task_id)
            .join("inbox")
            .join("worker")
    } else {
        run.runtime_dir.join("inbox")
    };
    let id = format!(
        "{}-{}",
        chrono::Utc::now().format("%Y%m%dT%H%M%S%.9f"),
        Uuid::new_v4()
    );
    atomic(
        &inbox.join(format!("{id}.json")),
        &json!({"id":id,"text":message,"at":now(),"source_run_id":run.id}),
    )
}

pub async fn stop(config: &Config, run: &AgentRun) -> Result<()> {
    let mut intent = validate_run(config, run)?;
    if let Some((pane, pid)) = find_pane(config, &run.id).await? {
        // Recover the pane option after a crash between new-window and set-option.
        tmux(
            config,
            &["set-option", "-p", "-t", &pane, "@int_agent_run", &run.id],
        )
        .await?;
        let condition = format!(
            "#{{&&:#{{==:#{{pane_pid}},{pid}}},#{{==:#{{@int_agent_run}},{}}}}}",
            run.id
        );
        let kill = format!("kill-pane -t {pane}");
        tmux(config, &["if-shell", "-F", "-t", &pane, &condition, &kill]).await?;
        ensure!(
            find_pane(config, &run.id).await?.is_none(),
            "owned pane did not stop"
        );
    }
    intent.phase = "stopped".into();
    atomic(&launch_path(run), &intent)
}

pub async fn cancel_task(config: &Config, task: &Task) -> Result<()> {
    for launch in launches(config, task)? {
        stop(config, &launch.run).await?;
    }
    Ok(())
}

async fn clean(worktree: &Path) -> Result<()> {
    let status = command::run(
        "git",
        &[
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignored",
        ],
        worktree,
    )
    .await?;
    ensure!(
        status.trim().is_empty(),
        "worktree {} has dirty, untracked, or ignored files; preserving it",
        worktree.display()
    );
    Ok(())
}
async fn remove_owned(config: &Config, owner: &mut Ownership, path: &Path) -> Result<()> {
    if !owner.worktree.exists() {
        owner.removed = true;
        atomic(path, owner)?;
        return Ok(());
    }
    verify_owner(config, owner).await?;
    clean(&owner.worktree).await?;
    git(config, &["worktree", "remove", text(&owner.worktree)?]).await?;
    owner.removed = true;
    atomic(path, owner)
}

pub async fn archive(config: &Config, task: &Task) -> Result<()> {
    task_root(config, task)?;
    // Fetch, then bind the proof to the actual remote integration tip, not stale local main.
    let reference = format!("refs/heads/{}", config.default_branch);
    let tracking = format!("refs/remotes/{}/{}", config.remote, config.default_branch);
    let refspec = format!("{reference}:{tracking}");
    git(config, &["fetch", "--no-tags", &config.remote, &refspec]).await?;
    let branch = format!("refs/heads/{}", task.branch);
    let tip = git(config, &["rev-parse", "--verify", &branch])
        .await?
        .trim()
        .to_owned();
    let integrated = command::output(
        "git",
        &["merge-base", "--is-ancestor", &tip, &tracking],
        &config.repository,
    )
    .await?;
    ensure!(
        integrated.code == 0,
        "task branch has commits not present in remote integration branch; preserving worktree"
    );
    let mut owners = Vec::new();
    let worker_path = owner_path(config, task);
    if worker_path.exists() {
        owners.push((worker_path.clone(), read_json::<Ownership>(&worker_path)?));
    }
    // Include reviewer manifests even when launch failed before an AgentRun reached SQLite.
    let root = config.task_dir(&task.id);
    if root.exists() {
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with("reviewer-owner-")
                && entry.path().extension().and_then(|s| s.to_str()) == Some("json")
            {
                owners.push((entry.path(), read_json::<Ownership>(&entry.path())?));
            }
        }
    }
    for launch in launches(config, task)? {
        let path = launch.run.runtime_dir.join("worktree-owner.json");
        if path.exists() {
            let owner: Ownership = read_json(&path)?;
            if !owners
                .iter()
                .any(|(_, prior)| prior.worktree == owner.worktree)
            {
                owners.push((path, owner));
            }
        }
    }
    for (_, owner) in &owners {
        ensure!(
            owner.task_id == task.id,
            "foreign worktree owner during archive"
        );
        if owner.worktree.exists() {
            verify_owner(config, owner).await?;
            clean(&owner.worktree).await?;
        }
    }
    cancel_task(config, task).await?;
    // A worker might have committed or dirtied its tree between the first proof and stop.
    let after = git(config, &["rev-parse", "--verify", &branch]).await?;
    ensure!(
        after.trim() == tip,
        "task branch changed while stopping; preserving all worktrees"
    );
    for (path, owner) in &mut owners {
        remove_owned(config, owner, path).await?;
    }
    atomic(
        &root.join("archive.json"),
        &json!({"at":now(),"branch":task.branch,"branch_tip":tip,"remote_main":git(config, &["rev-parse", "--verify", &tracking]).await?.trim()}),
    )
}

pub async fn attach(config: &Config) -> Result<()> {
    ensure_server(config).await?;
    let status = tokio::process::Command::new(&config.tmux)
        .args([
            "-S",
            text(&config.tmux_socket())?,
            "attach-session",
            "-t",
            SESSION,
        ])
        .current_dir(&config.repository)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .await
        .context("attaching controller tmux session")?;
    ensure!(status.success(), "tmux attach failed: {status}");
    Ok(())
}

pub fn report_path(run: &AgentRun) -> PathBuf {
    run.runtime_dir.join("review-report.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn fixture() -> Result<(tempfile::TempDir, Config, Task)> {
        let directory = tempfile::tempdir()?;
        let repository = directory.path().join("repo");
        fs::create_dir(&repository)?;
        command::run("git", &["init", "-b", "main"], &repository).await?;
        command::run(
            "git",
            &[
                "-c",
                "user.name=Runtime test",
                "-c",
                "user.email=runtime@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "--allow-empty",
                "-m",
                "Initial",
            ],
            &repository,
        )
        .await?;
        let base = command::run("git", &["rev-parse", "HEAD"], &repository).await?;
        let config: Config = serde_json::from_value(json!({
            "repository": repository,
            "worktree_root": directory.path().join("worktrees"),
            "state_dir": directory.path().join("state")
        }))?;
        let task: Task = serde_json::from_value(json!({
            "id": "runtime-task", "state": "starting",
            "spec": {
                "request_id": "runtime-task", "title": "Preserve user work", "goal": "Preserve user work",
                "acceptance": ["No uncommitted or foreign work is deleted"], "base_sha": base.trim(),
                "branch_slug": "preserve"
            },
            "branch": "worker/runtime-task-preserve",
            "worktree": config.worktree_root.join("runtime-task"),
            "post_ci": [], "created_at": now(), "updated_at": now()
        }))?;
        Ok((directory, config, task))
    }

    #[tokio::test]
    async fn dirty_owned_worktree_is_never_removed() -> Result<()> {
        let (_directory, config, task) = fixture().await?;
        let mut owner = prepare_worker(&config, &task).await?;
        let evidence = task.worktree.join("uncommitted-evidence.txt");
        fs::write(&evidence, "valuable uncommitted work")?;
        assert!(
            remove_owned(&config, &mut owner, &owner_path(&config, &task))
                .await
                .is_err()
        );
        assert_eq!(fs::read_to_string(evidence)?, "valuable uncommitted work");
        assert!(!owner.removed);
        Ok(())
    }

    #[tokio::test]
    async fn switched_worktree_is_not_silently_reclaimed() -> Result<()> {
        let (_directory, config, task) = fixture().await?;
        prepare_worker(&config, &task).await?;
        command::run("git", &["switch", "-c", "human-work"], &task.worktree).await?;
        assert!(prepare_worker(&config, &task).await.is_err());
        assert_eq!(
            command::run("git", &["branch", "--show-current"], &task.worktree)
                .await?
                .trim(),
            "human-work"
        );
        Ok(())
    }
}
