# 可观测性与可信本机边界

本文独立说明本机只读看板的字段、来源、更新时机与不可观测项。它不是交付成功证明，也不是操作控制台。安装和完整交付流程见 [README](../README.md)。

仓库中的文档是 `docs/observability.zh-CN.md`；Web 入口为 `/docs/observability.zh-CN`（默认 `http://127.0.0.1:8765/docs/observability.zh-CN`）。Web 将同一份 Markdown 作为 `text/plain; charset=utf-8` 内嵌提供，不渲染 Markdown、不读取任意磁盘文件；更新该文档后须重新构建 / 安装二进制才能更新 Web 内嵌副本。入口遵守与页面相同的 GET、本机 Host / Origin 和 CSP 边界。

## 1. 先决定审批模式，再启动

`approval_mode` 默认是 `write`，`int-agent.example.json` 也明确保留 `write`。可以选择 `always-ask`；只有你明确接受可信本机风险时，才在**实际配置文件中、启动 daemon 和新 Worker / Reviewer 之前**设为 `"approval_mode": "yolo"`。这是现有 JSON 配置，不存在新增的 `int-agent-mcp --yolo` 开关。

启用前必须理解：

- `yolo` 自动授权 omp 的工具调用，不再逐次等待原生工具权限确认，但不是隔离。所有运行仍在同一 Unix 用户权限下，可能读写该用户可访问的文件、执行命令、访问网络、使用 Git 配置及共享 GitHub 凭据，造成删除文件、推送代码或其他远端副作用。独立 worktree 不隔离主目录、文件系统、数据库、端口、网络或凭据；Reviewer 的独立工作区也不是安全沙箱。
- 原生 `ask` 是向用户提问，不是工具权限确认。`yolo` 不回答问题，不消除所有人工等待。用户仍须回原生窗口处理。
- PR CI、固定 base / head Review、精确 head 授权、串行集成与精确 merge SHA 的合并后 CI 是控制器的独立门禁；`yolo` 不跳过它们。不过同用户进程持有共享凭据时，控制器不能阻止其绕过控制器直接操作 GitHub。
- `manual` 表示需要显式授权调用，不是保证真人独占审批；有权调用授权 Tool 的 Agent 也能调用。强制人审、隔离不可信代码，应使用独立用户 / 容器、受限凭据及 GitHub 服务端规则。

配置不热重载。前台 daemon 可用 `Ctrl-C` 正常停止，再用同一配置启动；这不取消任务，也不停止独立 tmux / omp。重启后新配置只用于**实际新启动的 omp 运行实例**，不会修改存活进程的审批模式。恢复若仅重连原进程，仍保留原模式；恢复若新建进程，才使用新配置。不要为切换模式盲目取消或重复启动任务，也不要在运行中改仓库 / 状态 / worktree 根目录。

## 2. 四种证据必须分开

| 界面概念 | 真实含义 | 不能推出的结论 |
| --- | --- | --- |
| **任务交付阶段** | daemon 最近保存到 SQLite 的 `task.state` | 进程现在在线、模型正在输出、没有人工等待 |
| **当前运行观测 · 仅所选任务** | Web 对所选任务的 Worker / Reviewer 进行一次 tmux 与 `/proc` 采样，再由浏览器判断新鲜度 | 全团队在线总量、持续存活保证、任务完成 |
| **运行记录 · 工作区与会话历史** | 当前 Task 中各角色最近持久化的 `AgentRun`、目录与会话位置 | 所有历史 run 的完整列表、当前会话仍活着 |
| **PR · Review · CI 版本证据** | daemon 保存的固定版本远端证据、结构化报告及事件 | 浏览器已实时查询 GitHub、旧 head 的 PASS 可用于新 head |

“当前运行观测”的计数包括“采样确认在线”“本次未运行”“未知·旧观测”“运行记录”。范围仅为当前所选任务的两个角色记录，不是大管家人数或全队在线人数。任务索引只是数据库摘要，未采样实际进程，不计在线。角色职责图也是静态说明，不是实时拓扑。

持久化是**恢复线索**，不是 liveness（存活）证明。一个 `running` 任务可能已经退出，一个 `review` 任务的 Reviewer 可能在等待用户，一个 `completed` 任务仍可能保留运行实例；应分别核对阶段、采样时间、待答记录和版本证据。

## 3. 权威来源、写入者与读取链

看板不是 SQLite 查看器，也不是终端或 GitHub 的完整镜像。

| 来源 / 数据 | 写入者 | 读取者与更新时机 | 权威范围 |
| --- | --- | --- | --- |
| `state_dir/team.sqlite3` 的 `tasks` / `events` | daemon 通过 `Store::insert` / `save` | daemon 的 `list` / `get`；Web 通过 Unix IPC 查询 | 最近已提交的任务约定、阶段、元数据与控制器事件 |
| 每个 run 的 `launch.json`、worktree 所有权清单与 Git 管理目录的 `int-agent-owner.json` | runtime | 启动、重连、刷新或相应资源操作时 | 控制器记录的运行 / 资源归属与恢复意图，不是在线证据 |
| `runtime_dir/observer.json` | omp 内加载的 observer 扩展 | runtime 在刷新 / 就绪等待时读取，并把 PID / 会话路径更新到 Task | 最近一次扩展回调报告的运行元数据 |
| `runtime_dir/pending.json` | observer 在原生 `ask` 调用前写入，结束时删除 | daemon 的 `get` 直接读取；runtime 刷新另做身份与就绪校验 | 一个 ask 的待答标记，不覆盖原生权限弹窗 |
| `runtime_dir/observer-events.jsonl` | observer，逐条追加并同步 | 人工本地审计 | 扩展自己的回调审计，不等于 SQLite 事件流 |
| tmux 专用 socket 与 Linux `/proc` | tmux / 内核提供当前状态；runtime 设置运行环境及 pane 标记 | Web 在详情请求时只读采样；daemon 用自己的 runtime 刷新逻辑 | 采样时刻的进程归属证据 |
| `runtime_dir/review-report.json` | Reviewer 按任务约定写严格 JSON | daemon 在对应交付阶段读取、校验并持久化到 `task.review_report` | 该 Reviewer 对固定 base / head 的报告，不是代码正确性的证明 |
| 任务级 `inbox/worker/*.json` 与 `.ack` | daemon 写消息；observer 写回合结束确认 | observer 在空闲且无待答 / 待处理消息时轮询 | 排队、尝试交付、回合结束各自的局部证据 |
| GitHub PR、checks、Actions run、merge commit | GitHub 及真实仓库操作者 | daemon 在交付推进时用 `gh` 查询并记录 | 查询时特定版本的远端事实；Web 不自行实时查远端 |
| 浏览器缓存 | 当前页面脚本 | API 成功后更新，失败保留旧值与错误 | 该页面已读取片段，不是持久数据库或完整审计副本 |

### 非原子读取

链路为 **SQLite → daemon Unix IPC → Web 响应 → 浏览器**。即使单次 `Store::save` 在 SQLite 事务里同时提交 Task 与事件，也没有覆盖全部链路的原子快照：

1. daemon 的 `get` 先读 Task，再读事件页，再读两个 pending 文件；这些文件由独立 omp 进程更新。
2. Web 收到 IPC 后，另行查询 tmux 与 `/proc`，再生成响应时间。
3. 浏览器串行读取连接、任务页、详情；各响应来自不同时刻。事件历史还可能只加载了一部分。

因此任务阶段、pending、PID、事件、列表与详情可能短暂不一致；不能把一个响应内相邻字段误认为同一瞬间的事务事实。`observed_at` 是服务端一次读取 / 采样的时间标签，不是每个底层事实发生的时间，也不是持续心跳。

## 4. 字段与 UI 标签对照

### 4.1 控制器、任务索引与任务约定

| 原始字段 | UI 标签 / 展示 | 来源与解释 |
| --- | --- | --- |
| `result.version`、`repository`、`socket` | 控制器连接：版本、仓库、Socket | daemon `ping` 返回配置与二进制版本；成功只证明本次可响应 |
| `tasks[].id`、`title`、`state`、`branch` | 任务索引：任务 ID、标题、阶段、分支 | SQLite 分页摘要；不是实时进程扫描 |
| `tasks[].worker` / `reviewer` | 有运行记录 / 未创建 | 仅表示摘要是否持有角色运行记录 |
| `last_error_excerpt` | 索引错误摘录 | `last_error` 前 500 字符，不是完整错误或实时报警 |
| `next_after` | 上一页 / 下一页 | daemon 游标；看板每页至多 25 项 |
| `integration_blockers` | 全局集成占位 / 阻塞 | daemon 从独立全局活动集合返回 `merging`、`post_merge_ci`、`integration_blocked` 的任务 ID；不受当前页筛选影响，不代表全部失败 |
| `task.spec.goal`、`acceptance`、`non_goals` | 目标、验收标准、非目标 | 创建时的任务约定，不是已实现 / 已验收声明 |
| `spec.request_id` | request_id | 幂等请求关联标识，不是创建者身份 |
| `spec.base_sha`、`branch_slug`、`model` | 基线 SHA、branch_slug、模型请求 | 约定中的基线与模型请求；未指定模型不代表没有模型 |
| `task.branch`、`worktree` | 任务分支、任务 worktree | 控制器分配的资源位置，不提供隔离 |
| `created_at`、`updated_at` | 创建时间、记录更新时间 | daemon 持久化时刻；不是最后模型输出或心跳时间 |
| `spec.repairs_task_id`、`task.repaired_by` | 修复哪个任务、被哪个任务修复 | 明确的修复关系，不是任意任务依赖图 |
| `spec.completes_original_goal` | 修复承担原目标 | 任务约定；true 不等于验收已通过，恢复 main 不天然完成原功能 |
| `task.last_error` | 任务最后错误记录 | 最近保存的错误线索，不自动解释当前故障原因 |

看板不追踪大管家 / 创建者的具体会话、进程、身份或人数。不能从 `request_id`、任务标题、窗口名推造创建链与等待依赖。

### 4.2 全部任务阶段

阶段来自 `model::TaskState`，由 daemon 及交付状态机保存，不由 Web 根据 PID 或自然语言推断。

| `task.state` | UI 标签 | 解释边界 |
| --- | --- | --- |
| `starting` | 启动中 | 启动记录不等于已出现真实 `session_start` |
| `running` | 任务执行中 | 不证明进程此刻存活、模型忙碌或没有权限等待 |
| `waiting_user` | 记录为待用户 | Worker 刷新时发现有效 ask 标记；具体问题仍需看记录与原生窗口 |
| `pr_ci` | PR CI | 在推进检查，不等于已通过 |
| `review` | 独立审查 | 等待固定版本报告；Reviewer 是否在线另看采样 |
| `changes_requested` | 要求修改 | 有返工要求，不等于反馈已消费或已修复 |
| `ready_to_merge` | 待合并 | 仍需精确 head 授权（manual）、重验及串行门禁 |
| `merging` | 合并中 | 持久集成意图，不是已查证 merge commit |
| `post_merge_ci` | 合并后 CI | 核对实际 merge SHA 的 push CI，不等于成功 |
| `integration_blocked` | 集成阻塞 | 主分支验证 / 集成受阻，不能靠取消抹去 |
| `completed` | 已完成 | 控制器已记录交付通过，仍不等于进程退出或归档成功 |
| `archived` | 已归档 | 安全归档后的记录，仍保留会话与事件证据 |
| `cancelled` | 已取消 | 停止自己的运行实例并保留资源 / 历史，不是删除所有数据 |
| `failed` | 失败 | 持久错误阶段，不以旧 PID 判断当前状态 |
| `needs_attention` | 需关注 | 检查错误、运行身份与现场，不自动推断原因 |

### 4.3 Worker / Reviewer 运行记录与采样

`task.worker` / `task.reviewer` 为 `AgentRun` 或 null；各自是 Task 当前保存的角色运行记录，不是全部历史 run 列表。旧运行目录与会话可能仍保留在磁盘。

| 字段 | UI 标签 | 含义 |
| --- | --- | --- |
| `id`、`role` | Run ID、记录角色 | 运行 UUID 与 worker / reviewer 角色 |
| `window`、`pane` | 历史窗口 / pane | 持久记录的位置，不保证窗口仍存在，也不能靠名称证明身份 |
| `pid` | 历史记录 PID | observer 元数据经 daemon 保存的 PID；可能已经退出或复用 |
| `started_at` | 运行开始 | 运行记录的开始时间，不是在线时长保证 |
| `worktree`、`runtime_dir`、`session_dir` | 完整工作区与持久目录 | 资源与证据位置，不表示当前存在或正在运行 |
| `session_file` | 会话文件 | observer 报告的会话路径；初始惰性持久化时路径已分配但 JSONL 可能尚未落盘 |
| `observations.worker/reviewer.state` | 当前采样状态卡片 | `running`、`not_running`、`unknown`，与 `task.state` 独立 |
| `reason`、`observed_at` | 观测原因、进程观测时间 | 本次采样解释与时间；读失败时保留旧时间，不伪造新成功 |
| `pane_pid`、`agent_pid` | 采样 pane PID、采样 Agent PID | 仅在采样身份核验通过时提供，不从历史 PID 直接照抄认定在线；旧观测的 PID 也只是历史采样 |
| `terminal_command`（在 `observations` 下） | 复制团队会话接入命令 / 复制团队命令（非原窗口保证） | 参数逐项 shell 引用，仅复制，网页不执行 |

采样状态映射：

- `running`：新鲜时显示“采样确认在线”。Web 已核对专用 tmux socket、`int-agent` session、持久 pane、pane 的 `@int_agent_run`、pane 与 Agent 的实际 `/proc/PID/environ` 中 `INT_AGENT_RUN_ID`，并通过 `/proc/PID/stat` 检查 Agent 属于该 pane 的进程树；随后再核对 pane 身份以缩小退出 / PID 复用竞态。仍只是采样时刻的证据，不保证下一刻存活。
- `not_running`：成功列出 panes 后未找到记录中的所属 pane，或 pane 已标为退出，显示“本次未运行·历史记录”。这是对所属 pane 的观测，不是宣称全机没有其他同类进程，更不是完成信号。
- `unknown`：tmux 查询失败、格式或运行身份不符、PID 缺失、`/proc` 不可读、进程祖先归属无法核实等，显示“当前未知·不计在线”。不得把未知改成在线，也不得直接当作已退出。
- 采样 / 响应超过 30 秒，或详情读取失败，显示“旧观测·当前未知”，不计入在线。浏览器结合服务端时间和请求开始后的单调计时判断，把请求耗时也保守计入年龄，避免延迟返回的成功响应重新点亮旧实例；按秒及页面可见性变化复核。这不是精确的分布式时钟保证，慢请求也可能被保守降为未知。
- 没有 run 记录显示“无运行记录”，不补造进程。只有列表摘要而未读详情时，也不能计在线。旧版本若未提供观测或时间，按未知 / 旧观测处理，不升级为在线；本次无需数据库迁移或新增协议字段。

Web 的观测是只读路径，不调用会改写 `launch.json` 的 `runtime::refresh`。daemon 自己的刷新逻辑另行校验 launch UUID、角色与私有 runtime 目录，查找含匹配运行环境的实际 pane，并核对 observer 的 `run_id`、cwd 在所属 worktree 内、sessionFile 在所属会话目录内；ready 还要求实际进程身份与扩展就绪一致。两条路径用途不同，不应把它们当成同一个在线信号。

复制的命令接入团队 session，不保证进入旧 run 的窗口；窗口可能已消失。自行核对后在终端执行，用 `Ctrl-b w` 选择现场；已处于其他 tmux 时先脱离以避免嵌套。不要使用 daemon 响应中旧的、未逐项引用的顶层 `terminal_command`，网页只使用 `observations` 的引用版本。

### 4.4 observer 元数据、ask 与原生权限确认

`observer.json` 包含 `run_id`、`pid`、`sessionFile`、`cwd`、`ready`、`at`。由主 Agent 的 `session_start`、ask 返回、`agent_end`、`session_switch` 等回调更新；`session_shutdown` 写 `ready: false`。它不是定期心跳。真实 `session_start` 就绪不证明模型完成了一轮，也不保证 session JSONL 已落盘。

`worker_pending` / `reviewer_pending` 对应 UI“存在 ask 待答记录”“记录时间”“记录 Run ID”“工具调用 ID”“原始提问输入”“完整待答记录”。记录包含 `run_id`、`at`、`toolCallId`、`input`、`sessionFile`、`cwd`。

- observer 在 `ask` 的 pre-call 回调写 `pending.json` 并设置 tmux 注意标记 / 通知，所以记录可能**早于实际弹窗显示**。通知失败不等于没有问题，也没有离线手机 / 桌面推送保证。
- ask 工具返回、回合结束、会话启动或退出等会清理标记；返回可能是错误、取消、重定向或结束，不能当作“人类已经回答”。崩溃还可能留下旧 pending。
- daemon `get` 直接读取文件，不把它认证为当前有效等待；runtime 刷新只有在 ready 且 `run_id` 匹配时才据此判断等待。网页比较 pending 的 `run_id` 与角色记录，不一致时警告，不能当作当前等待证据。
- **原生工具权限确认弹窗不可观测。pending 为 null 只表示这次没有读到 ask 记录，不表示没有等待。** `running` 进程也可能正在等权限确认。
- 正确处理方式是回对应原生窗口核对并回答。网页、observer 和控制器不代答，不自动选择，也不以反馈消息冒充人类答案。

`observer-events.jsonl` 保存 `ask_pending`、`ask_result`、`ask_cleared`、会话与 inbox 等本地审计。当前 `get` 的 `events` 来自 SQLite，**没有把 observer 审计文件自动导入 SQLite 的通道**；前端即使认识同名事件标签，也不说明所有 observer 事件会出现在网页时间线。需要完整上下文时，结合原生窗口、准确的会话文件及该审计文件只读检查。

### 4.5 返工、报告与版本证据

| 字段 / 事件 | UI 标签 | 证明范围 |
| --- | --- | --- |
| `worker_feedback_queued` | 返工反馈已排队 | 控制器保存了反馈意图 |
| `worker_feedback_delivered` | 返工反馈已写入 inbox | `runtime::send_message` 已写持久消息；不证明 Worker 已读取、执行、修复或回答用户 |
| observer `inbox_delivery` / `inbox_ack` | 尝试交付 / 回合结束确认（审计语义） | 扩展在空闲时送入 follow-up，或该回合结束写 ack；不是语义验收通过，且不自动进入 SQLite 时间线 |
| `pr.number/url/base_sha/head_sha` | Pull Request、PR base SHA、PR head SHA | daemon 保存的 PR 身份和版本 |
| `review_base` / `review_head` | 审查固定 base / head | 独立 Reviewer 本轮审查版本 |
| `review_report` | 结构化审查报告 | 报告 base/head、`verdict`、`summary`、`findings`、`verification` |
| `merge_authorized_head` | 合并授权 head | 只授权精确版本，不是免除 CI / Review |
| `merge_sha` | 实际 merge SHA | 查证的真实合并版本，不能用 PR head 代替 |
| `post_ci[]` | 合并后 CI 证据 | `workflow`、`run_id`、`head_sha`、`url`、`conclusion`；需对应实际 merge SHA |
| SQLite `pr_ci_*` 事件 | PR CI 版本证据 · 已加载 | 事件当时的版本和检查数据；旧记录不能证明当前 head 通过 |

Reviewer 应在该 run 的 `review-report.json` 原子写严格 JSON。daemon 限制报告大小、解析 `ReviewReport`，核对 base / head 与本轮 PR 一致、摘要非空、验证条目非空后才保存报告并推进阶段。缺失、不完整、过期报告会等待或拒绝，Reviewer 退出不等于审查通过。报告的验证描述是 Agent 提交的证据，不是网页重新运行了验证，也不是数学正确性证明。

返工消息的 1 秒轮询只在 observer 已有上下文、空闲、无待答、无在途消息且无待处理消息时尝试；不是每秒必达承诺。`delivered` 的命名指写入 inbox，不能解释为“已消费”或“已修复”。新的 head / base 使旧审查或授权失效；看板不以单个 PASS、旧事件或进程退出替代版本门禁。

## 5. 更新节奏、新鲜度与分页

- daemon 的 `poll_seconds` 默认 5，可配置 1–60 秒。每个 tick 只从活动任务中轮流推进**一项**，不是全队每 5 秒扫描一遍；外部命令、IPC 处理与活动任务数量都会影响某任务的实际刷新间隔。跳过错过的 tick，不承诺硬实时延迟。
- runtime 刷新把 observer 的 PID / 会话路径带入运行记录；元数据变化时保存 `session_metadata`。Worker 的 `starting` / `running` / `waiting_user` 转换在被推进时计算，Reviewer 等待则主要体现在其记录、pending、错误与交付阶段中，不把所有等待都编码成 `waiting_user`。
- worktree 的归属、Git 注册、所有权 token、分支或 detached commit 在相应创建 / 恢复 / 复用 / 删除等操作边界检查；不是每 tick 对全部 worktree、文件、分支、脏状态做完整扫描。路径字段存在不证明工作区当前完整或干净。
- 浏览器采用单一异步循环，状态、列表、详情串行，本轮完成约 5 秒后才开始下一轮；不是固定墙钟每 5 秒更新。刷新按钮只是排队读取。Web 的 daemon 查询超时为 10 秒，浏览器读取还有 45 秒中止保护；超时不取消底层任务。
- “控制器连接”“当前页成功观测”“详情成功观测”“进程观测”“事件页成功观测”各有时间。失败保留旧数据、原成功时间和错误，读取成功才清除相应错误；过期 / 失败采样不算当前在线。
- 看板任务页上限 25；状态筛选只作用于当前页，“本页读取 / 筛选显示”不等于全局任务数。切换页或筛选不清空已选任务，右侧详情可属于当前页之外。
- 详情每次读取至多 25 条事件，使用 `events_next_after` 继续；读到末尾后从已读最大 `sequence` 检查新事件。事件按 sequence 去重、升序保留在页面内存，切换任务或分页不清空；整页重载后重新读取。
- “事件时间线”“返工反馈与等待线索”及 PR CI 事件区只筛选**已加载历史**。`events_next_after: null` 只表示本次页没有更多后续，不是永远没有新事件；未见记录不等于未发生。`event_limit: 0` 的协议读取还可完全省略事件。

## 6. 恢复能力与当前缺口

已有机制包括 SQLite 事务、唯一 request_id、启动意图、运行身份、独立 tmux、持久会话与 inbox、取消意图和版本化交付门禁。daemon 停止、MCP 断开或 Web 关闭不自动取消独立 Worker；超时可能发生在操作已接受之后，应查询或按同一 request_id 重试，而不是盲目换 ID。

但当前**没有**以下保证：

- 没有 Agent / observer 定期心跳，没有最后 token / 最后输出活动时间或模型忙闲遥测；`updated_at`、`observer.at`、快照时间都不能替代心跳。
- 没有全量持续 reconciler，把数据库、所有 launch、全部 tmux / 进程、工作区与 GitHub 在每轮完整对账；已有恢复只是按任务 / 操作边界推进，不能推称“后台已全量核实”。
- 没有通用孤儿进程 / 孤儿 worktree 收集器，不自动认领或删除磁盘上任意遗留资源；保留记录或安全归档不等于全机无孤儿。
- 没有原生工具权限弹窗状态、用户是否看到问题 / 是否作答的完整证明，也没有大管家在线人数、创建者身份或任意任务依赖追踪。
- 没有从 observer 审计到 SQLite 的完整事件同步，没有全量会话渲染与独立实时 PR 检查矩阵；浏览器不是完整会话审计器。
- 没有多用户认证、跨用户隔离或凭据隔离。loopback、私有目录 / socket、CSP 与同源限制不是多租户安全边界。

遇到未知或冲突时保留“未知”，核对对应原生窗口、准确 run / 会话路径、Git 状态及版本化远端证据，不用旧 PID、空 pending、反馈 delivered 或持久化成功代替现场确认。不要启动第二个 `omp --resume` 作为旁观器；可用 `omp render /absolute/path/to/session.jsonl --plain` 只读查看已落盘会话。

## 7. 只读与暴露边界

Web 固定监听 `127.0.0.1`，只提供明确静态资源、该文档和 GET 查询 API，不提供创建、答题、权限批准、恢复、取消、授权合并、归档或终端执行接口；也不是任意 daemon 请求代理。它不自动启动 daemon、不自动修复连接，失败不补演示数据。

请求须满足本机 Host / Origin 边界，不启用 CORS；响应设置严格 CSP、禁止嵌框、`nosniff` 和不缓存。页面使用本地资源，记录按文本展示，外链仅接受 HTTP(S)。这些限制不意味着可以安全公开：任务内容、问题、错误、目录、会话及凭据线索可能敏感。不要通过反向代理、端口转发或共享浏览器暴露给不可信用户。

## 8. 实现依据

以下是本文陈述对应的实现入口，不是本次已运行验证的声明：

- [model.rs](../src/model.rs)：配置默认值、TaskState、Task / AgentRun / ReviewReport / Event、IPC 类型。
- [config.rs](../src/config.rs)、[main.rs](../src/main.rs)：审批模式校验、启动时加载配置和现有 CLI。
- [store.rs](../src/store.rs)：SQLite schema、任务摘要与事件分页、Task + 事件事务、持久取消意图。
- [service.rs](../src/service.rs)：单控制器、本机 IPC 读取、pending 读取、每 tick 一项任务、状态刷新和正常停止。
- [ipc.rs](../src/ipc.rs)：每次独立连接、JSONL 请求 / 响应、大小与时间限制、断开不发送取消。
- [runtime.rs](../src/runtime.rs)：launch / 所有权证据、tmux 环境身份、observer 刷新、审批模式传参、重连 / 新建、inbox 和安全归档。
- [observer.ts](../assets/observer.ts)：元数据与 ask 记录、原子写、独立审计、清理、空闲 inbox 轮询与 ack。
- [delivery.rs](../src/delivery.rs)：反馈写入语义、报告校验、PR CI / Review / merge / push CI 门禁。
- [web.rs](../src/web.rs)、[observation.rs](../src/web/observation.rs)：只读路由、边界、内嵌文档、IPC 包装与 tmux / `/proc` 采样。
- [app.js](../assets/web/app.js)、[index.html](../assets/web/index.html)：字段标签、阶段 / 进程分离、页面新鲜度、事件缓存与串行轮询。
