use std::{path::Path, process::Stdio, time::Duration};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

const OUTPUT_LIMIT: usize = 1024 * 1024;
const DEADLINE: Duration = Duration::from_secs(20);

#[derive(Debug)]
pub struct Output {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

async fn capture(mut stream: impl AsyncRead + Unpin) -> std::io::Result<(Vec<u8>, bool)> {
    let mut bytes = Vec::new();
    let mut overflow = false;
    let mut buffer = [0_u8; 8192];
    loop {
        let count = stream.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        let keep = count.min(OUTPUT_LIMIT.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&buffer[..keep]);
        overflow |= keep != count;
    }
    Ok((bytes, overflow))
}

pub async fn output(program: &str, args: &[&str], cwd: &Path) -> Result<Output> {
    let mut child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("launching {program} in {}", cwd.display()))?;
    let pid = child.id().context("subprocess has no pid")?;
    let stdout = child.stdout.take().context("missing stdout pipe")?;
    let stderr = child.stderr.take().context("missing stderr pipe")?;
    let mut out = tokio::spawn(capture(stdout));
    let mut err = tokio::spawn(capture(stderr));
    let result = tokio::time::timeout(DEADLINE, async {
        let status = child.wait().await?;
        let stdout = (&mut out).await.context("stdout reader failed")??;
        let stderr = (&mut err).await.context("stderr reader failed")??;
        Ok::<_, anyhow::Error>((status, stdout, stderr))
    })
    .await;
    let (status, (stdout, out_overflow), (stderr, err_overflow)) = match result {
        Ok(Ok(value)) => value,
        other => {
            // Each command gets a fresh process group. Kill descendants too, including
            // those retaining a pipe after their parent exited; never orphan timed-out git/gh.
            let _ = std::process::Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{pid}")])
                .status();
            let _ = child.kill().await;
            let _ = child.wait().await;
            out.abort();
            err.abort();
            match other {
                Err(_) => bail!("{program} exceeded its 20 second deadline"),
                Ok(Err(error)) => {
                    return Err(error).with_context(|| format!("reading {program} output"));
                }
                Ok(Ok(_)) => unreachable!(),
            }
        }
    };
    if out_overflow || err_overflow {
        bail!("{program} exceeded the 1 MiB per-stream output limit");
    }
    Ok(Output {
        code: status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

pub async fn run(program: &str, args: &[&str], cwd: &Path) -> Result<String> {
    let result = output(program, args, cwd).await?;
    if result.code != 0 {
        bail!(
            "{program} exited {}: {}{}",
            result.code,
            result.stderr,
            result.stdout
        );
    }
    Ok(result.stdout)
}
