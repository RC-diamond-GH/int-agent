mod command;
mod config;
mod delivery;
mod ipc;
mod mcp;
mod model;
mod runtime;
mod service;
mod store;

use std::{
    path::{Path, PathBuf},
    process::ExitCode,
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use model::{Request, Response};

#[derive(Parser)]
#[command(
    version,
    about = "Persistent independent omp Workers with an MCP steward interface"
)]
struct Cli {
    /// JSON configuration shared by the MCP gateway and independent daemon.
    #[arg(long, global = true, default_value = "int-agent.json")]
    config: PathBuf,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Serve newline-delimited MCP JSON-RPC on stdin/stdout (the default).
    Serve,
    /// Run the persistent Unix-socket controller independently of MCP clients.
    Daemon,
    /// Read one Request JSON from stdin and emit one Response JSON.
    Call,
    /// Attach to the native interactive Worker/Reviewer tmux session.
    Attach,
}

#[tokio::main]
async fn main() -> ExitCode {
    match run(Cli::parse()).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(error) => {
            eprintln!("{error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<bool> {
    let command = cli.command.unwrap_or(Command::Serve);
    if matches!(command, Command::Call) {
        return call(&cli.config).await;
    }
    let config = config::load(&cli.config)?;
    match command {
        Command::Serve => mcp::serve(&config).await?,
        Command::Daemon => service::run(config).await?,
        Command::Attach => runtime::attach(&config).await?,
        Command::Call => unreachable!("call handled before loading configuration"),
    }
    Ok(true)
}

async fn call(path: &Path) -> Result<bool> {
    let result = async {
        let config = config::load(path)?;
        let mut input = Vec::new();
        tokio::io::stdin()
            .take((ipc::MAX_REQUEST_BYTES + 1) as u64)
            .read_to_end(&mut input)
            .await
            .context("reading Request JSON from stdin")?;
        let request = parse_request(&input)?;
        ipc::request(&config.socket(), &request).await
    }
    .await;
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
    let mut output = serde_json::to_vec(&response).context("serializing CLI Response")?;
    output.push(b'\n');
    let mut stdout = tokio::io::stdout();
    stdout
        .write_all(&output)
        .await
        .context("writing CLI Response")?;
    stdout.flush().await.context("flushing CLI Response")?;
    Ok(response.ok)
}

fn parse_request(input: &[u8]) -> Result<Request> {
    if input.len() > ipc::MAX_REQUEST_BYTES {
        bail!("stdin Request exceeds the 1 MiB limit");
    }
    serde_json::from_slice(input).context("decoding stdin Request JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn call_requires_one_typed_request() {
        assert!(parse_request(br#"{"op":"list"} {"op":"ping"}"#).is_err());
        assert!(parse_request(br#"{"op":"list","unknown":true}"#).is_err());
        assert!(parse_request(&vec![b' '; ipc::MAX_REQUEST_BYTES + 1]).is_err());
    }
}
