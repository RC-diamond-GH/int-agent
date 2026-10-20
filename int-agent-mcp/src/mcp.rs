use std::path::PathBuf;

use anyhow::{Context, Result};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt,
    handler::server::wrapper::Parameters,
    model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::{
    ipc,
    model::{Config, Request, StartTask},
};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskId {
    /// Persistent task identifier returned by team_start_task.
    pub task_id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListTasks {
    /// Continue after the task ID returned as next_after.
    pub after: Option<String>,
    /// At most 100 task summaries; defaults to 25.
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GetTask {
    pub task_id: String,
    /// Continue after the event sequence returned as events_next_after.
    pub events_after: Option<i64>,
    /// At most 100 events; defaults to 25. Zero omits event history.
    pub event_limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AuthorizeMerge {
    /// Persistent task identifier returned by team_start_task.
    pub task_id: String,
    /// Exact reviewed PR head commit to authorize; never a branch name.
    pub expected_head: String,
}

#[derive(Clone)]
pub struct TeamServer {
    socket: PathBuf,
}

#[tool_router]
impl TeamServer {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    async fn forward(&self, request: Request) -> Result<CallToolResult, McpError> {
        match ipc::request(&self.socket, &request).await {
            Ok(value) => Ok(CallToolResult::structured(value)),
            Err(error) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "{error:#}"
            ))])),
        }
    }

    #[tool(
        description = "Start an independent interactive omp Worker in an isolated git worktree. Supply a stable request_id for idempotency, an exact base_sha, and concrete acceptance criteria. Tasks are rolling independent assignments, not a dependency DAG. The persistent daemon owns execution after acceptance."
    )]
    async fn team_start_task(
        &self,
        Parameters(spec): Parameters<StartTask>,
    ) -> Result<CallToolResult, McpError> {
        self.forward(Request::Start { spec }).await
    }

    #[tool(
        description = "List task summaries in creation order, 25 per page by default. Follow next_after until null so historical pages do not hide current Workers. Use team_get_task for full specifications and evidence. Direct human questions to the original Worker window; never answer on the human's behalf.",
        annotations(read_only_hint = true)
    )]
    async fn team_list_tasks(
        &self,
        Parameters(args): Parameters<ListTasks>,
    ) -> Result<CallToolResult, McpError> {
        self.forward(Request::List {
            after: args.after,
            limit: args.limit,
        })
        .await
    }

    #[tool(
        description = "Read a complete task, native Worker/Reviewer identities, CI/review state, errors and pending human questions. Event history is paginated: follow events_next_after until null; event_limit=0 omits history. Inspect the original native session for full conversation context.",
        annotations(read_only_hint = true)
    )]
    async fn team_get_task(
        &self,
        Parameters(args): Parameters<GetTask>,
    ) -> Result<CallToolResult, McpError> {
        self.forward(Request::Get {
            task_id: args.task_id,
            events_after: args.events_after,
            event_limit: args.event_limit,
        })
        .await
    }

    #[tool(
        description = "Cancel a task through the daemon and stop its owned Worker/Reviewer runs. This is an explicit task cancellation, unlike an MCP disconnect or cancellation of a tool request; evidence is preserved."
    )]
    async fn team_cancel_task(
        &self,
        Parameters(args): Parameters<TaskId>,
    ) -> Result<CallToolResult, McpError> {
        self.forward(Request::Cancel {
            task_id: args.task_id,
        })
        .await
    }

    #[tool(
        description = "Resume a recoverable persistent task using its existing task/worktree/session evidence. Does not answer a Worker's pending human question; use its native interactive window for that."
    )]
    async fn team_resume_task(
        &self,
        Parameters(args): Parameters<TaskId>,
    ) -> Result<CallToolResult, McpError> {
        self.forward(Request::Resume {
            task_id: args.task_id,
        })
        .await
    }

    #[tool(
        description = "Authorize merging only the exact expected_head commit for a task under manual merge policy. Authorization does not bypass required PR CI, independent review, base/head freshness, or the serialized integration gate."
    )]
    async fn team_authorize_merge(
        &self,
        Parameters(args): Parameters<AuthorizeMerge>,
    ) -> Result<CallToolResult, McpError> {
        self.forward(Request::AuthorizeMerge {
            task_id: args.task_id,
            expected_head: args.expected_head,
        })
        .await
    }

    #[tool(
        description = "Safely archive an eligible completed task after confirmed merge and exact merge-SHA push CI. The daemon verifies clean, remotely integrated worktrees before removing them; task, session, and run evidence is preserved."
    )]
    async fn team_archive_task(
        &self,
        Parameters(args): Parameters<TaskId>,
    ) -> Result<CallToolResult, McpError> {
        self.forward(Request::Archive {
            task_id: args.task_id,
        })
        .await
    }
}

#[tool_handler]
impl ServerHandler for TeamServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
            .with_instructions(
                "You are the steward of independent interactive omp Workers. Use these tools to issue rolling user/steward tasks and inspect persistent evidence; do not implement the coding work yourself, build dependency DAGs, or impersonate a Worker. Read task errors, CI, review, and native window evidence before reporting completion. When a Worker asks the human a question, notify the human and direct them to that Worker's native tmux window using `int-agent-mcp --config <path> attach`; never answer on their behalf. A separate daemon owns all accepted work: MCP disconnects and tool-request cancellations do not cancel tasks. Merge authorization is bound to an exact reviewed head and never bypasses gates."
            )
    }
}

pub async fn serve(config: &Config) -> Result<()> {
    // stdout belongs exclusively to newline-delimited MCP JSON-RPC.
    let service = TeamServer::new(config.socket())
        .serve(stdio())
        .await
        .context("initializing MCP stdio service")?;
    service.waiting().await.context("serving MCP stdio")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unavailable_daemon_is_a_tool_error() {
        let server = TeamServer::new(PathBuf::from("/nonexistent/int-agent/controller.sock"));
        let result = server
            .team_list_tasks(Parameters(ListTasks {
                after: None,
                limit: None,
            }))
            .await
            .unwrap();
        assert_eq!(result.is_error, Some(true));
    }
}
