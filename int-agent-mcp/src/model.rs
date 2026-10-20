use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub repository: PathBuf,
    pub worktree_root: PathBuf,
    pub state_dir: PathBuf,
    #[serde(default = "main_branch")]
    pub default_branch: String,
    #[serde(default = "origin")]
    pub remote: String,
    #[serde(default = "omp")]
    pub omp: String,
    #[serde(default = "tmux")]
    pub tmux: String,
    #[serde(default = "gh")]
    pub gh: String,
    #[serde(default)]
    pub worker_model: Option<String>,
    #[serde(default)]
    pub reviewer_model: Option<String>,
    #[serde(default)]
    pub allowed_models: Vec<String>,
    #[serde(default = "approval_mode")]
    pub approval_mode: String,
    #[serde(default = "poll_seconds")]
    pub poll_seconds: u64,
    #[serde(default = "max_workers")]
    pub max_workers: usize,
    #[serde(default)]
    pub github: Option<GitHubConfig>,
}
fn main_branch() -> String {
    "main".into()
}
fn origin() -> String {
    "origin".into()
}
fn omp() -> String {
    "omp".into()
}
fn tmux() -> String {
    "tmux".into()
}
fn gh() -> String {
    "gh".into()
}
fn approval_mode() -> String {
    "write".into()
}
fn poll_seconds() -> u64 {
    5
}
fn max_workers() -> usize {
    4
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubConfig {
    pub repository: String,
    pub required_checks: Vec<String>,
    pub post_merge_workflows: Vec<String>,
    #[serde(default)]
    pub merge_policy: MergePolicy,
    #[serde(default)]
    pub delete_remote_branches: bool,
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MergePolicy {
    #[default]
    Manual,
    Automatic,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct StartTask {
    pub request_id: String,
    pub title: String,
    pub goal: String,
    #[serde(default)]
    pub non_goals: Vec<String>,
    pub acceptance: Vec<String>,
    pub base_sha: String,
    pub branch_slug: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub repairs_task_id: Option<String>,
    /// A repair that only rolls back/restores main must leave this false.
    #[serde(default)]
    pub completes_original_goal: bool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Starting,
    Running,
    WaitingUser,
    PrCi,
    Review,
    ChangesRequested,
    ReadyToMerge,
    Merging,
    PostMergeCi,
    IntegrationBlocked,
    Completed,
    Archived,
    Cancelled,
    Failed,
    NeedsAttention,
}
impl TaskState {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Archived | Self::Cancelled | Self::Failed)
    }
    pub fn integration(self) -> bool {
        matches!(
            self,
            Self::Merging | Self::PostMergeCi | Self::IntegrationBlocked
        )
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentRun {
    pub id: String,
    pub role: String,
    pub worktree: PathBuf,
    pub pane: String,
    pub window: String,
    pub runtime_dir: PathBuf,
    pub session_dir: PathBuf,
    pub session_file: Option<PathBuf>,
    pub pid: Option<u32>,
    pub started_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub url: String,
    pub head_sha: String,
    pub base_sha: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewReport {
    pub base_sha: String,
    pub head_sha: String,
    pub verdict: ReviewVerdict,
    pub summary: String,
    pub findings: Vec<String>,
    pub verification: Vec<String>,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewVerdict {
    Pass,
    ChangesRequested,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CiEvidence {
    pub workflow: String,
    pub run_id: u64,
    pub head_sha: String,
    pub url: String,
    pub conclusion: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub spec: StartTask,
    pub state: TaskState,
    pub branch: String,
    pub worktree: PathBuf,
    pub worker: Option<AgentRun>,
    pub reviewer: Option<AgentRun>,
    pub pr: Option<PullRequest>,
    pub review_base: Option<String>,
    pub review_head: Option<String>,
    pub review_report: Option<ReviewReport>,
    pub merge_authorized_head: Option<String>,
    pub merge_sha: Option<String>,
    pub post_ci: Vec<CiEvidence>,
    pub repaired_by: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub sequence: i64,
    pub task_id: String,
    pub kind: String,
    pub at: String,
    pub data: serde_json::Value,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Ping {},
    Start {
        spec: StartTask,
    },
    List {
        #[serde(default)]
        after: Option<String>,
        #[serde(default)]
        limit: Option<u32>,
    },
    Get {
        task_id: String,
        #[serde(default)]
        events_after: Option<i64>,
        #[serde(default)]
        event_limit: Option<u32>,
    },
    Cancel {
        task_id: String,
    },
    Resume {
        task_id: String,
    },
    AuthorizeMerge {
        task_id: String,
        expected_head: String,
    },
    Archive {
        task_id: String,
    },
}
#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}
pub fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}
impl Config {
    pub fn socket(&self) -> PathBuf {
        self.state_dir.join("controller.sock")
    }
    pub fn tmux_socket(&self) -> PathBuf {
        self.state_dir.join("tmux.sock")
    }
    pub fn task_dir(&self, id: &str) -> PathBuf {
        self.state_dir.join("tasks").join(id)
    }
}
