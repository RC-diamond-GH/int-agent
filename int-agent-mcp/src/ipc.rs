use std::{path::Path, time::Duration};

use anyhow::{Context, Result, bail};
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::UnixStream,
    time::timeout,
};

use crate::model::{Request, Response};

pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
const REQUEST_DEADLINE: Duration = Duration::from_secs(120);

/// Each call owns one connection. Dropping this client never sends a task-cancel
/// operation: accepted operations belong to the independent daemon.
pub async fn request(socket: &Path, request: &Request) -> Result<Value> {
    let mut payload = serde_json::to_vec(request).context("serializing daemon request")?;
    if payload.len() >= MAX_REQUEST_BYTES {
        bail!("daemon request exceeds the 1 MiB JSONL limit");
    }
    payload.push(b'\n');

    timeout(REQUEST_DEADLINE, async {
        let mut stream = UnixStream::connect(socket)
            .await
            .with_context(|| format!("connecting to daemon at {}; run `int-agent-mcp --config <path> daemon` first", socket.display()))?;
        stream.write_all(&payload).await.context("sending daemon request")?;
        // Half-close only the request direction. The daemon can finish an
        // accepted operation even if this client subsequently disconnects.
        stream.shutdown().await.context("finishing daemon request")?;
        let mut reader = BufReader::new(stream).take((MAX_RESPONSE_BYTES + 1) as u64);
        let mut line = Vec::new();
        reader.read_until(b'\n', &mut line).await.context("reading daemon response")?;
        if line.len() > MAX_RESPONSE_BYTES {
            bail!("daemon response exceeds the 8 MiB JSONL limit");
        }
        if line.is_empty() {
            bail!("daemon disconnected without a response; an accepted operation may still be running");
        }
        if line.last() != Some(&b'\n') {
            bail!("daemon response ended without a JSONL newline");
        }
        let response: Response = serde_json::from_slice(&line).context("decoding daemon Response JSON")?;
        if response.ok {
            if response.error.is_some() {
                bail!("daemon sent an inconsistent successful response containing an error");
            }
            Ok(response.result.unwrap_or(Value::Null))
        } else {
            bail!("{}", response.error.as_deref().unwrap_or("daemon operation failed without an error message"));
        }
    })
    .await
    .context("daemon request exceeded its 120 second deadline; an accepted operation may still be running")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    async fn exchange(reply: Vec<u8>) -> Result<Value> {
        let directory = tempfile::tempdir()?;
        let socket = directory.path().join("controller.sock");
        let listener = UnixListener::bind(&socket)?;
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            stream.read_to_end(&mut request).await.unwrap();
            // A limit rejection can close the connection before the write ends.
            let _ = stream.write_all(&reply).await;
        });
        let result = request(&socket, &Request::Ping {}).await;
        server.await?;
        result
    }

    #[tokio::test]
    async fn preserves_daemon_errors() {
        let error = exchange(b"{\"ok\":false,\"error\":\"task not found\"}\n".to_vec())
            .await
            .unwrap_err();
        assert!(error.to_string().contains("task not found"));
    }

    #[tokio::test]
    async fn rejects_truncated_and_malformed_responses() {
        assert!(exchange(b"{\"ok\":true}".to_vec()).await.is_err());
        assert!(exchange(b"not json\n".to_vec()).await.is_err());
        assert!(exchange(Vec::new()).await.is_err());
    }

    #[tokio::test]
    async fn rejects_oversized_response() {
        let reply = vec![b'x'; MAX_RESPONSE_BYTES + 1];
        let error = exchange(reply).await.unwrap_err();
        assert!(error.to_string().contains("8 MiB"));
    }

    #[tokio::test]
    async fn rejects_oversized_request_before_connecting() {
        let error = request(
            Path::new("/nonexistent/int-agent.sock"),
            &Request::Get {
                task_id: "x".repeat(MAX_REQUEST_BYTES),
                events_after: None,
                event_limit: None,
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("1 MiB"));
    }
}
