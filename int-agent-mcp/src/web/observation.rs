use std::{fs, path::Path};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    command,
    model::{AgentRun, Config, now},
};

#[derive(Serialize)]
struct Observation {
    state: &'static str,
    reason: String,
    observed_at: String,
    pane_pid: Option<u32>,
    agent_pid: Option<u32>,
    terminal_command: String,
}

// Deserialize only the run identities rather than copying the task goal and
// event payloads returned by IPC.
#[derive(Deserialize)]
pub(super) struct Runs {
    worker: Option<AgentRun>,
    reviewer: Option<AgentRun>,
}

pub(super) async fn inspect(config: &Config, task: &Runs) -> Value {
    if task.worker.is_none() && task.reviewer.is_none() {
        return json!({"worker":null,"reviewer":null});
    }
    // Never use runtime::refresh: it persists launch metadata. This command only
    // lists panes on the configured team's dedicated socket, without starting it.
    let socket = config.tmux_socket();
    let panes = match socket.to_str() {
        Some(socket) => {
            command::run(
                &config.tmux,
                &[
                    "-S",
                    socket,
                    "list-panes",
                    "-a",
                    "-F",
                    "#{session_name}\t#{pane_id}\t#{pane_pid}\t#{pane_dead}\t#{@int_agent_run}",
                ],
                &config.repository,
            )
            .await
        }
        None => Err(anyhow::anyhow!("tmux socket 路径不是 UTF-8")),
    };
    let terminal = terminal_command(config);
    let observe = |run: &AgentRun| {
        let command = terminal.clone();
        match panes.as_ref() {
            Ok(panes) => match inspect_run(run, panes) {
                Ok((state, reason, pane_pid, agent_pid)) => Observation {
                    state,
                    reason,
                    observed_at: now(),
                    pane_pid,
                    agent_pid,
                    terminal_command: command,
                },
                Err(error) => unknown(format!("{error:#}"), command),
            },
            Err(error) => unknown(format!("专用 tmux socket 探测失败：{error:#}"), command),
        }
    };
    json!({"worker":task.worker.as_ref().map(&observe),"reviewer":task.reviewer.as_ref().map(observe)})
}

fn unknown(reason: String, terminal_command: String) -> Observation {
    Observation {
        state: "unknown",
        reason,
        observed_at: now(),
        pane_pid: None,
        agent_pid: None,
        terminal_command,
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}
fn terminal_command(config: &Config) -> String {
    // Attach the team session, not a potentially stale/recycled historical pane.
    // Quote every argument, including configured executable and paths.
    [
        &config.tmux,
        "-S",
        &config.tmux_socket().to_string_lossy(),
        "attach-session",
        "-t",
        "int-agent",
    ]
    .map(shell_quote)
    .join(" ")
}

type ProcessObservation = (&'static str, String, Option<u32>, Option<u32>);

fn inspect_run(run: &AgentRun, panes: &str) -> Result<ProcessObservation> {
    uuid::Uuid::parse_str(&run.id).context("run_id 不是合法 UUID，无法核对身份")?;
    ensure!(
        run.pane.starts_with('%')
            && run.pane.len() > 1
            && run.pane[1..].bytes().all(|b| b.is_ascii_digit()),
        "持久化 pane 标识无效"
    );
    let mut matching = None;
    for line in panes.lines() {
        let mut columns = line.split('\t');
        let fields = match std::array::from_fn::<_, 5, _>(|_| columns.next()) {
            [Some(session), Some(pane), Some(pid), Some(dead), Some(id)] => {
                [session, pane, pid, dead, id]
            }
            _ => bail!("tmux 返回的 pane 观测格式无法解析"),
        };
        ensure!(columns.next().is_none(), "tmux 返回的 pane 观测列过多");
        if fields[0] == "int-agent" && fields[1] == run.pane {
            ensure!(matching.is_none(), "tmux 返回重复 pane，无法核对身份");
            matching = Some(fields);
        }
    }
    let Some(fields) = matching else {
        return Ok((
            "not_running",
            "本次成功查询未找到持久化身份所属 pane；不代表任务已完成，会话历史仍保留".into(),
            None,
            None,
        ));
    };
    if fields[3] == "1" {
        return Ok((
            "not_running",
            "tmux 将所属 pane 标记为已退出；Task 状态与会话历史独立保留".into(),
            None,
            None,
        ));
    }
    ensure!(fields[3] == "0", "tmux pane_dead 字段无效");
    ensure!(
        fields[4] == run.id,
        "pane run_id 与持久化运行身份不符，不能确认归属"
    );
    let pane_pid: u32 = fields[2].parse().context("tmux pane PID 无效")?;
    verify_process(pane_pid, &run.id).context("核对 pane 真实进程身份失败")?;
    let agent_pid = run
        .pid
        .context("未记录 Agent PID，不能据 pane 推断 Agent 存活")?;
    verify_process(agent_pid, &run.id).context("核对已记录 Agent PID 的当前身份失败")?;
    ensure!(
        descends_from(agent_pid, pane_pid)?,
        "Agent PID 不属于该 pane 的当前进程树"
    );
    // Recheck the pane identity after walking ancestry to narrow exit/PID-reuse
    // races. This remains a point-in-time observation, never a completion signal.
    verify_process(pane_pid, &run.id)?;
    Ok((
        "running",
        "已核对专用 socket、所属 pane、run_id、实际 pane/Agent PID 与进程归属；仅为采样时刻观测"
            .into(),
        Some(pane_pid),
        Some(agent_pid),
    ))
}

fn verify_process(pid: u32, id: &str) -> Result<()> {
    ensure!(pid > 0, "PID 必须大于零");
    let bytes = fs::read(format!("/proc/{pid}/environ"))
        .with_context(|| format!("无法读取 /proc/{pid}/environ（可能退出或无权限）"))?;
    let expected = format!("INT_AGENT_RUN_ID={id}");
    ensure!(
        bytes
            .split(|b| *b == 0)
            .any(|entry| entry == expected.as_bytes()),
        "当前 PID 不含匹配的 INT_AGENT_RUN_ID，不能用数据库 PID 判定存活"
    );
    Ok(())
}
fn descends_from(mut pid: u32, ancestor: u32) -> Result<bool> {
    for _ in 0..64 {
        if pid == ancestor {
            return Ok(true);
        }
        if pid <= 1 {
            return Ok(false);
        }
        pid = parent_pid(Path::new(&format!("/proc/{pid}/stat")))?;
    }
    bail!("进程祖先链超过观测上限，归属未知")
}
fn parent_pid(path: &Path) -> Result<u32> {
    let stat = fs::read_to_string(path).with_context(|| format!("读取 {} 失败", path.display()))?;
    // comm can contain spaces and parentheses; only its last ')' precedes state.
    stat.rsplit_once(')')
        .context("/proc stat 缺少 comm 结束符")?
        .1
        .split_whitespace()
        .nth(1)
        .context("/proc stat 缺少 PPID")?
        .parse()
        .context("/proc stat PPID 无效")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Read,
        process::{Command, Stdio},
    };

    #[test]
    fn validates_actual_process_environment_not_saved_pid() {
        let id = uuid::Uuid::new_v4().to_string();
        let mut child = Command::new("sh")
            .args(["-c", "printf ready; read -r line"])
            .env("INT_AGENT_RUN_ID", &id)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        // spawn's exec-pipe close can precede /proc's new environment becoming
        // visible. Wait for userspace readiness, then keep this process blocked
        // on its private stdin instead of racing a second exec or sleeping.
        let mut ready = [0; 5];
        child.stdout.take().unwrap().read_exact(&mut ready).unwrap();
        let matching = verify_process(pid, &id);
        let mismatched = verify_process(pid, "different-run");
        let ancestry = descends_from(pid, std::process::id());
        child.kill().unwrap();
        child.wait().unwrap();
        matching.unwrap();
        assert!(mismatched.is_err());
        assert!(ancestry.unwrap());
        assert!(verify_process(pid, &id).is_err());
    }

    #[test]
    fn recycled_pane_with_different_run_never_appears_running() {
        let run: AgentRun = serde_json::from_value(json!({"id":uuid::Uuid::new_v4().to_string(),"role":"worker","worktree":"/unused","pane":"%2","window":"worker","runtime_dir":"/unused","session_dir":"/unused","session_file":null,"pid":std::process::id(),"started_at":"unused"})).unwrap();
        assert!(
            inspect_run(
                &run,
                &format!("int-agent\t%2\t{}\t0\tdifferent-run\n", std::process::id())
            )
            .is_err()
        );
        assert_eq!(
            inspect_run(&run, "int-agent\t%2\t0\t1\told-run\n")
                .unwrap()
                .0,
            "not_running"
        );
        assert_eq!(inspect_run(&run, "").unwrap().0, "not_running");
    }
}
