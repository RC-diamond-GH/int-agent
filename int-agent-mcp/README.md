# int-agent-mcp

在单台 Ubuntu、一个终端里运行以 omp 为核心的敏捷 Agent 团队。大管家通过 MCP 下发当前任务；独立 daemon 创建 worktree、启动真正的交互式 omp Worker，保存状态并执行交付门禁。大管家换会话或 MCP 断开不会取消 Worker。

## 组成

- `serve`：官方 Rust MCP SDK 的 stdio 服务，只做类型化工具入口与本机 IPC；stdout 专用于 MCP。
- `daemon`：唯一控制器，通过私有 Unix socket 接收操作，SQLite 保存任务与事件，tmux 承载独立 omp 进程。
- Worker：一项任务、一条分支、一个 worktree、独立持久会话。不是大管家的短生命周期子代理。
- Reviewer：固定 base/head SHA 的 detached worktree、独立交互式 omp、结构化审查报告。
- 交付：PR CI → 独立 Review → 串行 merge → 精确 merge SHA 的 push CI → 安全归档。不调度预测 DAG。

本地控制器不代替 Agent 研究和实现，也不根据自然语言中出现一个“PASS”就放行。

## 安装

需要 Rust 1.90+、Git、tmux、已配置模型认证的 omp。GitHub 自动交付还需要近期版本 `gh` 和相应仓库权限。

以下安装命令在仓库的 `int-agent-mcp/` 目录执行。

```bash
sudo apt update
sudo apt install git tmux
omp --version
gh --version
gh auth status
cargo install --path . --locked
```

`gh` 建议使用 [官方 Ubuntu 安装源](https://github.com/cli/cli/blob/trunk/docs/install_linux.md#debian)，不要依赖可能缺少新选项的旧发行版包。omp 认证使用 `omp login`；模型 ID 从 `omp models` 获取。程序不会替你安装软件、创建 GitHub 仓库、购买套餐、配置保护规则或保存模型密钥。

## 配置

复制 `int-agent.example.json`，替换所有路径、仓库和真实 CI 检查名称。路径必须绝对，repository、worktree_root、state_dir 必须彼此不包含。仓库必须已有初始提交；不要把状态或 worktree 放进产品仓库。

```bash
mkdir -p "$HOME/.config/int-agent"
cp int-agent.example.json "$HOME/.config/int-agent/team.json"
# 使用编辑器填写配置后再启动。
int-agent-mcp --config "$HOME/.config/int-agent/team.json" daemon
```

主要配置：

| 字段 | 含义 |
| --- | --- |
| `repository` | 大管家探索的主工作区根目录 |
| `worktree_root` | 仅供本团队创建 worktree 的外部目录 |
| `state_dir` | 私有状态目录，SQLite、socket、任务卡、会话、运行身份及归档证据 |
| `default_branch` / `remote` | 默认 `main` / `origin` |
| `omp` / `tmux` / `gh` | 可执行文件名或绝对路径，不是 shell 命令字符串 |
| `worker_model` / `reviewer_model` | 可选；省略时使用 omp 自身配置 |
| `allowed_models` | MCP 调用者可以选择的模型；空数组禁止任意模型覆盖，配置的 `worker_model` 仍可选择 |
| `approval_mode` | 默认 `write`；执行工具需用户批准，可明确选择 `always-ask` 或可信环境下的 `yolo` |
| `poll_seconds` | 控制器轮询间隔 1–60 秒；任务逐项推进 |
| `max_workers` | 活跃任务上限 1–32；关联修复可以替代其阻塞父任务的活跃名额 |
| `github.repository` | 真实 `OWNER/REPOSITORY`，须与 Git remote 指向同一仓库 |
| `github.required_checks` | PR 必须真实成功的检查名称，不接受空集合或 skipped/cancelled/neutral |
| `github.post_merge_workflows` | 必须验证的 workflow **文件名或数字 ID**，例如 `ci.yml`，不是任意显示名称 |
| `github.merge_policy` | `manual` 默认，需要精确 head 授权；`automatic` 仍必须满足全部 CI/Review/串行门禁 |
| `github.delete_remote_branches` | 默认 false，远端分支保留为历史；启用后仅在验证归档条件后清理 |

可以完全省略 `github`，用于本地 Worker 管理。此时任务不会假装通过 CI 或完成合并，不能自动标记交付完成；需要实际交付时配置真实 GitHub。不要用空成功脚本充当 `gh`。

配置文件不热重载；修改后重启 daemon 与 MCP 接口，已有任务和 omp 进程保留。不要在任务执行中修改仓库/状态/worktree 根目录或模型运行环境。

### 长期运行 daemon

可以在当前终端自己的 tmux 窗口启动 daemon，也可以使用 systemd 用户服务。例：保存为 `~/.config/systemd/user/int-agent.service`，先确认可执行文件与配置位置：

```ini
[Unit]
Description=Local omp team controller

[Service]
ExecStart=%h/.cargo/bin/int-agent-mcp --config %h/.config/int-agent/team.json daemon
Environment=PATH=%h/.cargo/bin:%h/.bun/bin:/usr/local/bin:/usr/bin:/bin
Restart=on-failure
KillMode=process
UMask=0077

[Install]
WantedBy=default.target
```

```bash
systemctl --user daemon-reload
systemctl --user enable --now int-agent.service
journalctl --user -u int-agent.service -f
```

`KillMode=process` 是有意的：停止控制器不连带杀掉独立 tmux/omp。停止整个团队要显式取消未合并任务；已进入集成的任务需要完成验证/修复，不能靠取消掩盖主分支故障。系统重启仍会终止进程，但任务与会话可以恢复。

## 接入大管家的 omp

在**大管家的** `.omp/mcp.json` 或其专用 profile 的 `mcp.json` 添加：

```json
{
  "mcpServers": {
    "int-agent": {
      "type": "stdio",
      "command": "/home/you/.cargo/bin/int-agent-mcp",
      "args": ["--config", "/home/you/.config/int-agent/team.json", "serve"],
      "timeout": 180000
    }
  }
}
```

替换绝对路径；现有 MCP 配置要合并，不要覆盖。配置参考 [omp MCP 文档](omp://mcp-config.md)。控制器不会因为 MCP 初始化自动启动，也不会因为 MCP 退出停止。

大管家的角色约定：

```text
你是团队大管家：探索仓库、研究、与用户澄清，只下发当前已经明确的任务。
不要编写产品代码，不把后续设想变成自动执行 DAG。
通过 team_start_task 创建独立 Worker，任务必须包含目标、非目标、完整基线 SHA 与验收标准。
request_id 对同一任务保持稳定；若修改任务约定，使用新的 request_id，而不是复用旧标识。
通过 team_list_tasks/team_get_task 读取真实结果，不能把进程启动或 PR 合并当成交付完成。
Worker 直接向人提问，你只提示用户进入它的原生窗口，不替用户回答。
每轮实际交付后重新检查 main 与证据，再决定下一轮。
只有用户明确授权你承担集成人职责，才调用 team_authorize_merge；否则把待合并任务交给用户。
```

MCP 的 `manual` 是“需要显式授权调用”，**不是不可绕过的人类权限边界**：有权调用授权 Tool 的 Agent 也可以发起授权。要强制真人审批，需在客户端审批或 GitHub 独立身份/规则处实施；同一 Unix 用户和共享凭据不是安全隔离。

## 工具

| MCP Tool | 参数/行为 |
| --- | --- |
| `team_start_task` | 任务约定；幂等创建并返回实际启动状态 |
| `team_list_tasks` | `after`、`limit` 可选；分页任务摘要与集成阻塞项 |
| `team_get_task` | `task_id`；完整任务与待答问题；`events_after`、`event_limit` 分页读取事件 |
| `team_cancel_task` | `task_id`；持久化取消意图并停止自己的运行实例，保留代码与证据 |
| `team_resume_task` | `task_id`；重连/恢复原 Worker 或固定版本 Reviewer，不重复分配任务 |
| `team_authorize_merge` | `task_id`、`expected_head`；只授权已审查版本，不绕过任何门禁 |
| `team_archive_task` | `task_id`；重新尝试归档已完成的任务，不强删脏工作区 |

列表默认每页 25 项，最多 100 项，继续传回 `next_after`，直到它为 null。列表省略长目标和完整审查内容，完整约定仍由 `team_get_task` 返回。事件默认每页 25 项、最多 100 项，并有字节预算；用 `events_next_after` 继续，`event_limit: 0` 可只读任务而不取事件。不会因为归档历史增长就一次返回全部任务和全部上下文。

`team_start_task` 示例（SHA 和目标必须填写真实内容）：

```json
{
  "request_id": "import-validation-001",
  "title": "校验导入文件格式",
  "goal": "无效格式返回明确错误，不产生部分写入；保持有效导入兼容",
  "non_goals": ["不重写导入架构"],
  "acceptance": ["有效文件仍能导入", "损坏文件返回错误且数据库不变"],
  "base_sha": "完整的真实 Git commit SHA",
  "branch_slug": "import-validation"
}
```

`model` 可选，受配置允许列表约束；`repairs_task_id` 可选，只能关联一个主分支集成失败的原任务。修复任务的 `completes_original_goal` 默认 false：只恢复 main（包括回退）不会完成原功能；只有明确承担并验收原目标时才设为 true。相同 request_id 和相同约定返回原任务；同一 request_id 的不同约定被拒绝。分支和路径由控制器分配，调用者不能传任意 shell 或任意目录。

## 不经过 MCP 的终端操作

`call` 使用同样的 daemon 协议，读取一个 JSON，输出 `{ok,result,error}`；失败退出码非零，适合人工操作和故障排查：

```bash
CFG="$HOME/.config/int-agent/team.json"
printf '%s\n' '{"op":"ping"}' | int-agent-mcp --config "$CFG" call
printf '%s\n' '{"op":"list"}' | int-agent-mcp --config "$CFG" call
printf '%s\n' '{"op":"get","task_id":"实际任务ID"}' | int-agent-mcp --config "$CFG" call
int-agent-mcp --config "$CFG" attach
```

启动任务的终端协议为 `{"op":"start","spec":{...上述约定...}}`，其余操作为 `cancel`、`resume`、`authorize_merge`、`archive`。授权操作额外包含 `expected_head`。

## 人类提问与上下文审计

`attach` 进入这个团队自己的 tmux server；`Ctrl-b w` 选择 Worker/Reviewer，`Ctrl-b d` 脱离，进程继续运行。如果已在别的 tmux 中，先脱离当前客户端再 attach，避免嵌套。

受控运行的 omp 加载随二进制发布的 observer 扩展：

- 实际 `session_start` 之后才写就绪与准确 sessionFile，状态才可从 `starting` 进入 `running`。
- `ask` 发起时保存问题、为窗口设置 `[ASK]`，在接入的终端提示；`ask.timeout=0`，不自动代选。
- 用户回到原 Worker 界面回答；控制器不代理生成答案。
- 完成、取消、错误或回合结束清理待答标志；失败不是“用户已回答”。

扩展的 pre-call 事件早于弹窗实际显示，`[ASK]` 表示需要注意，不保证弹窗已经就绪。进程崩溃可能留下记录；以运行身份和实际 UI 为准。脱离终端后没有手机/桌面推送服务。

omp 的普通新会话在首次 assistant 消息前可能仍是惰性持久化：`sessionFile` 路径已分配，但 JSONL 暂未出现。`running` 表示真实 omp 会话已就绪，不表示模型已经完成一轮或会话文件一定已落盘。此时先查看原生窗口和控制器保存的任务卡；首次 assistant 消息/工具调用持久化后，再用下面的只读命令查看。恢复没有产生历史的初始运行会重新附上同一任务约定，不伪造丢失的对话。

用 `team_get_task` 找到准确会话路径，在另一个操作窗口只读查看：

```bash
omp render /absolute/path/to/session.jsonl --plain | less
```

同时检查 Worker 的实际 `git status`、已暂存/未暂存 diff 及相关未跟踪文件。不要用第二个 `omp --resume` 当旁观器；不要在待答弹窗上直接敲 `/tree` 或用 Esc 假装“挂起”。需要时用 `Alt-h` 收起问题或 tmux copy-mode 查看历史，再回原问题回答。

CI/Review 的返工反馈通过本地持久 inbox 交给空闲 Worker；不是向问答弹窗盲目发送按键，不会把反馈当成人类答案。

## GitHub 交付流程

### 前置规则

- Git remote 与 `github.repository` 指向同一仓库，默认分支与配置一致。目前仅支持直接指向 `github.com` 的 HTTPS、`git@github.com:` 或 `ssh://git@github.com/` 地址；不同 `GH_HOST` 或 SSH 主机别名会明确拒绝，不会误操作另一个仓库。
- 真实 workflow 同时覆盖 PR 和默认分支 push，检查名与 workflow 文件名填写准确。
- 保护默认分支：PR、必需检查、strict up-to-date、禁止 force-push 和删除；允许 merge commit，不启用 merge queue。
- 不允许其他身份/工具同时合并：本 daemon 的串行锁不能约束 GitHub 外部合并者。
- 不使用 `pull_request_target` 检出并运行未审查代码，不用 skipped/neutral/空工作流冒充成功。

### 状态推进

1. Worker 运行真实验证，提交、推送当前任务分支，创建面向配置默认分支的同仓库 PR。
2. 控制器发现 PR，确认当前 head/base 及每个配置检查真实成功；存在 test-merge 检查时按 GitHub 的合并测试提交判断，并验证其父提交确实为当前 base/head，否则使用 head 检查。CI 证据保存具体受测 SHA，合并后不会误把新 push CI 当作原 PR CI。缺失和 pending 等待，失败交原 Worker 返工。依据：[GitHub 检查来源规则](https://docs.github.com/en/pull-requests/how-tos/merge-and-close-pull-requests/troubleshooting-required-status-checks#conflicts-between-head-commit-and-test-merge-commit)。
3. 在固定 head 的 detached worktree 启动独立 Reviewer，检查 diff、相关实现、测试、安全、性能、架构与语义验收。
4. Reviewer 写严格 JSON 报告，包含 base/head、`PASS` 或 `CHANGES_REQUESTED`、摘要、发现、实际验证。报告必须匹配本轮版本；新提交或 base 变化使旧结论和授权失效。
5. 审查通过发布带版本的 PR comment。单账户方案不伪装成 GitHub 独立 Approval；需要 required approval 时，另外提供真正不同的审查身份。
6. `manual` 策略等待对应 head 的授权；`automatic` 自动进入相同门禁。合并前重新确认 SHA、CI、审查和独占集成条件，持久化 `merging` 后执行带 `--match-head-commit` 的 merge，不加 `--admin`/`--auto`。
7. 查到实际 merge commit 后，按该精确 SHA、push 事件、默认分支和每个指定 workflow 验证。每个 run 要有真实成功 job，缺失/pending 不算成功，失败进入 `integration_blocked`。
8. 全部成功才完成并尝试安全归档。归档拒绝脏目录、未跟踪/ignored 文件、额外未合并提交；常见的依赖和构建目录也需要用户明确清理后再重试，不强删。保留本地分支引用、任务目录和原始会话作为证据；可选的远端分支清理另有精确 SHA 的 lease 保护。

Reviewer 的 JSON 是 Agent 提供的证据，不是对代码正确性的数学证明。Review 不信任代码里的提示注入，报告需要实际验证记录，产品验收仍由任务约定、Worker、Reviewer 和用户共同承担。

### 主分支失败

`integration_blocked` 阻止其他普通任务合并。创建带 `repairs_task_id` 的修复任务，从包含失败合并的真实 main SHA 开始；它仍完整走 CI → Review → merge → push CI。修复成功会关联并解除原任务的集成阻塞，保留原始失败提交和证据。`completes_original_goal=false` 时原任务进入 `needs_attention`，不归档；为 true 且实际交付通过时才进入完成流程。修复自身再次破坏 main 时允许显式关联下一级修复；只有这条修复链可以通过相关阻塞，其他任务仍不得合并。

自动流程不擅自 reset/回退产品变更。若决定回退，明确下发回退任务、关联原任务，仍走同样门禁。**恢复主分支不天然等于原功能交付完成**：回退后由大管家重新规划功能任务，不能把回退任务的成功当作原功能验收。

主分支恢复后，大管家在干净的主工作区上 `fetch` 并 `merge --ff-only` 已验证的 main，再开始新一轮规划；不要继续读取旧工作区代码。控制器不自动改动大管家的当前目录，以免与研究命令或用户修改冲突。

## 持久化与恢复

- task + request_id 唯一约束保证重复调用不会多创建一个 Worker；不同约定复用标识会失败。
- 启动记录和所有权证据先于进程副作用保存；恢复优先重连原运行实例，不仅检查一个可能被复用的 PID。
- daemon 重启后重新读取任务、运行证据与 GitHub 真实状态；`merging`/主分支失败不会因为一次查询错误变成普通失败而解除锁。
- MCP 请求超时/断开可能发生在操作已经接受之后；用同 request_id 重试或查询，不盲目换 ID。
- 取消有持久意图，异常或重启后继续停止目标；取消保留 worktree、分支、会话和事件。已合并任务不允许取消来掩盖故障。
- 归档不强删，不清空整个 worktree 根目录。会话、原始运行目录及外部 artifact 保留，HTML 或一份摘要不是完整备份。
- omp 的共享 blob store 和模型认证仍由所用 profile 管理；备份原始状态时也需考虑引用的 blob。程序不会复制你的全部凭据或整个主目录。

## 权限边界

这是可信本机同用户的协作控制器，不是多租户沙箱。worktree 不隔离文件系统、Git 配置、数据库、端口、网络或凭据；MCP 工具描述也不是权限系统。Worker 应只看到它需要的 MCP 服务，团队 MCP 建议只配置在大管家的专用 profile；不保证提示词能阻止同用户进程自行访问控制 socket。

`approval_mode=write` 的执行审批需要用户处理；授予某工具本会话许可会扩大能力。`yolo` 不是安全加固。运行不可信代码、真正限制谁能 merge、强制独立 Review 时，使用独立用户/容器、受限凭据及 GitHub 服务端规则。

## 开发与验证

```bash
cd int-agent-mcp # 从仓库根目录进入 crate
cargo +1.90.0 fmt --all -- --check
cargo +1.90.0 clippy --locked --all-targets -- -D warnings
cargo +1.90.0 test --locked
cargo build --locked
```

测试关注幂等持久化、取消恢复、协议边界、版本门禁和安全状态转换。单元测试不代替真实 MCP 握手、daemon 重启、tmux/omp 就绪与 GitHub 交付演练。

仓库根目录的 [`.github/workflows/ci.yml`](../.github/workflows/ci.yml) 可由 GitHub Actions 发现：面向 main 的 PR 和 main push 均运行真实的格式检查、Clippy 与行为测试，job ID 和检查名保持为 `verify`。工作流在 checkout 后安装 Rust 1.90.0（含 rustfmt 和 Clippy），所有 `run` 步骤的工作目录为 `int-agent-mcp/`，`contents` 权限仅为只读。把本项目作为受控仓库时，示例配置的 `verify` / `ci.yml` 与它对应；尚未配置远端保护规则或尚未运行的 CI 不算已通过。

本地实施验证：

- Rust 行为测试覆盖持久化幂等性、取消重启、截断协议帧、历史分页、过期审查、CI 状态、修复链，以及拒绝删除脏/切换分支的 worktree。
- 实际 MCP initialize / tools/list / tools/call 创建了真实 omp Worker；MCP 断开不取消 Worker。
- 实际强杀并重启 daemon 后重连原 Worker；Worker 进程崩溃后恢复持久会话，未提交文件保留；取消只停止自己的运行实例，代码与记录保留。
- 实际模型发出的原生 ask 在另一个 tmux 窗口触发通知与 `[ASK]`，等待期间未自动回答；只读审计后在原生 UI 选择了 `Preserve compatibility`，真实 ask 结果及待答清理均已观察。
- 85 个各含 100000 字节目标的历史任务曾触发 8 MiB IPC 上限；修复后通过真实 daemon 分 4 页取回，最大响应 9361 字节，未丢任务。
- 未创建或修改真实远端 PR、保护规则、合并与 Actions run；完整 GitHub 交付仍需在你授权的真实仓库演练，不能把单元测试或只读 API 探查当作远端验收。
