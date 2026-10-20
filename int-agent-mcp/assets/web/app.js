"use strict";

(() => {
  const $ = id => document.getElementById(id);
  const STATES = {
    starting: "启动中", running: "任务执行中", waiting_user: "记录为待用户", pr_ci: "PR CI",
    review: "独立审查", changes_requested: "要求修改", ready_to_merge: "待合并",
    merging: "合并中", post_merge_ci: "合并后 CI", integration_blocked: "集成阻塞",
    completed: "已完成", archived: "已归档", cancelled: "已取消", failed: "失败", needs_attention: "需关注"
  };
  const STAGE_NOTES = {
    starting: "控制器记录为启动阶段；需结合会话就绪、实际进程与错误证据确认现场。",
    running: "控制器记录为任务执行阶段；不证明模型正在输出，也不排除人工权限等待。",
    waiting_user: "控制器记录为待用户阶段；以下 ask 记录与原生窗口才能说明具体问题。",
    pr_ci: "控制器记录为 PR CI 阶段；等待固定版本的必需检查，不代表检查已经通过。",
    review: "控制器记录为独立审查阶段；以固定 base / head 及结构化报告为证据。",
    changes_requested: "控制器记录为需要返工；反馈进入 inbox 不代表 Worker 已消费或完成修改。",
    ready_to_merge: "控制器记录为待合并；仍受精确 head 授权、版本重验与串行集成门禁约束。",
    merging: "控制器记录为合并中；占用全局集成门禁，不能仅凭请求已发送判定已合并。",
    post_merge_ci: "控制器记录为合并后验证；必须核对实际 merge SHA 的 push CI。",
    integration_blocked: "控制器记录为集成阻塞；需检查主分支证据与明确修复关联，不能靠取消视为解决。",
    completed: "控制器记录为已完成；仍应核对合并与精确版本 CI 证据，不代表进程已退出或已归档。",
    archived: "控制器记录为已归档；保留运行、会话与事件证据，并非删除全部历史。",
    cancelled: "控制器记录为已取消；保留资源与历史，当前进程状态仍以下方观测为准。",
    failed: "控制器记录为失败；最后错误与历史事件提供线索，不能由旧 PID 推断当前进程。",
    needs_attention: "控制器记录为需关注；检查错误、运行身份和原生现场，不自动推断故障原因。"
  };
  const EVENT_NAMES = {
    created: "任务已创建", starting: "启动记录", worker_started: "Worker 已启动",
    worker_ready: "Worker 会话就绪", reviewer_started: "Reviewer 已启动",
    ask_pending: "提问待处理", ask_result: "提问工具返回", ask_cleared: "待答标记清理",
    worker_feedback_queued: "返工反馈已排队", worker_feedback_delivered: "返工反馈已写入 inbox",
    inbox_delivery: "Observer 尝试交付反馈", inbox_ack: "Observer 回合结束确认",
    delivery_waiting: "交付等待", ready_to_merge: "进入待合并", review_passed: "审查通过记录",
    changes_requested: "审查要求修改", completed: "完成记录", cancelled: "取消记录",
    archived: "归档记录", integration_blocked: "集成阻塞记录",
    pr_ci_passed: "PR CI 通过记录", pr_ci_reconfirmed: "PR CI 重新确认",
    pr_ci_before_merge: "合并前 PR CI 重验", pr_ci_at_merge: "合并时 PR CI 记录",
    pr_ci_waiting: "PR CI 等待", pr_ci_failed: "PR CI 失败",
    review_report_received: "收到结构化审查报告", reviewer_observed: "Reviewer 运行记录",
    post_merge_ci_observed: "合并后 CI 观测", integration_completed: "集成完成记录"
  };
  const state = {
    status: null, statusError: "", list: null, listError: "", pageStack: [null], pageIndex: 0,
    requestedPage: null, filter: "all", selected: null, tasks: new Map(), eventFilter: "all",
    jobs: { status: false, list: false, detail: false }, running: false, timer: null,
    detailSignature: "", copyTimer: null
  };

  function element(tag, className, text) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== undefined) node.textContent = String(text);
    return node;
  }
  function text(value, fallback = "未记录") {
    if (value === null || value === undefined || value === "") return fallback;
    return typeof value === "object" ? JSON.stringify(value, null, 2) : String(value);
  }
  function json(value) { return JSON.stringify(value, null, 2) ?? "null"; }
  function array(value) { return Array.isArray(value) ? value : []; }
  function time(value) {
    if (!value) return "未记录时间";
    const date = new Date(value);
    return Number.isNaN(date.valueOf()) ? String(value) : date.toLocaleString("zh-CN", { hour12: false });
  }
  function timestamp(value) {
    const node = element("time", "", time(value));
    if (value) { node.dateTime = value; node.title = value; }
    return node;
  }
  function freshness(id, at, prefix = "成功观测") {
    const target = $(id);
    target.replaceChildren();
    if (!at) { target.textContent = "尚无成功观测"; return; }
    target.append(`${prefix}：`, timestamp(at), " · 失败时保留此时间与旧数据");
  }
  function error(id, message) {
    const target = $(id);
    target.hidden = !message;
    target.textContent = message;
  }
  function tone(value) {
    if (["completed", "archived", "running", "PASS", "success"].includes(value)) return "good";
    if (["failed", "integration_blocked", "CHANGES_REQUESTED", "failure"].includes(value)) return "danger";
    if (["waiting_user", "needs_attention", "changes_requested", "unknown", "not_running", "cancelled"].includes(value)) return "warning";
    return "neutral";
  }
  function badge(label, value) { return element("span", `badge ${tone(value)}`, label); }
  function fields(entries) {
    const dl = element("dl");
    for (const [label, value, mono] of entries) {
      const dd = element("dd", mono ? "mono" : "");
      dd.append(value instanceof Node ? value : text(value));
      dl.append(element("dt", "", label), dd);
    }
    return dl;
  }
  function valuesList(values, emptyMessage) {
    if (!array(values).length) return element("p", "empty", emptyMessage);
    const list = element("ul");
    for (const value of values) list.append(element("li", "text-block", text(value)));
    return list;
  }
  function safeLink(raw, label) {
    try {
      const url = new URL(raw);
      if (!["http:", "https:"].includes(url.protocol)) throw new Error("unsafe protocol");
      const link = element("a", "", label || url.href);
      link.href = url.href;
      link.target = "_blank";
      link.rel = "noopener noreferrer";
      return link;
    } catch {
      return element("span", "muted", raw ? `${text(raw)}（非 HTTP(S) 地址，不建立链接）` : "未记录链接");
    }
  }
  function selectLink(id) {
    if (!id) return element("span", "muted", "未记录");
    const button = element("button", "inline-link mono", id);
    button.type = "button";
    button.addEventListener("click", () => selectTask(id));
    return button;
  }
  function section(title, key, collapsible = false, open = true) {
    const node = element(collapsible ? "details" : "section", "evidence-section");
    if (collapsible) { node.open = open; node.dataset.disclosure = key; }
    node.append(element(collapsible ? "summary" : "h3", "", title));
    return node;
  }
  function notice(message) { return element("p", "notice", message); }
  function entry(id) {
    if (!state.tasks.has(id)) state.tasks.set(id, {
      response: null, error: "", events: new Map(), nextAfter: null, lastSequence: 0,
      hasEventPage: false, eventObservedAt: null
    });
    return state.tasks.get(id);
  }

  async function get(path) {
    const controller = new AbortController();
    const timeout = setTimeout(() => controller.abort(), 45000);
    try {
      const response = await fetch(path, {
        method: "GET", headers: { Accept: "application/json" }, credentials: "same-origin",
        cache: "no-store", signal: controller.signal
      });
      const contentType = response.headers.get("content-type") || "";
      if (!contentType.includes("application/json")) throw new Error(`HTTP ${response.status}：服务未返回 JSON`);
      const data = await response.json();
      if (!response.ok) throw new Error(`HTTP ${response.status}：${text(data.error, "读取失败")}${data.observed_at ? `（失败观测 ${time(data.observed_at)}）` : ""}`);
      if (!data || !data.result || !data.observed_at) throw new Error("响应缺少结果或观测时间");
      return data;
    } catch (failure) {
      if (failure.name === "AbortError") throw new Error("浏览器读取超过 45 秒，已中止本次读取；上次成功快照保持不变。");
      throw failure;
    } finally { clearTimeout(timeout); }
  }

  function request(jobs) {
    for (const job of jobs) state.jobs[job] = true;
    clearTimeout(state.timer);
    if (!state.running) void pump();
  }
  async function pump() {
    if (state.running) return;
    state.running = true;
    $("poll-state").textContent = "正在串行读取本机观测；不会并发轮询或执行控制操作。";
    $("refresh").textContent = "刷新 / 排队读取";
    try {
      while (Object.values(state.jobs).some(Boolean)) {
        const job = ["status", "list", "detail"].find(name => state.jobs[name]);
        state.jobs[job] = false;
        if (job === "status") await readStatus();
        if (job === "list") await readList();
        if (job === "detail" && state.selected) await readDetail(state.selected);
      }
    } finally {
      state.running = false;
      $("refresh").textContent = "刷新观测";
      $("poll-state").textContent = "自动观测：本轮结束约 5 秒后再读取；请求串行，不重叠。时间为服务端观测时间，并非持续在线保证。";
      state.timer = setTimeout(() => request(state.selected ? ["status", "list", "detail"] : ["status", "list"]), 5000);
    }
  }
  async function readStatus() {
    try {
      state.status = await get("/api/status");
      state.statusError = "";
    } catch (failure) { state.statusError = failure.message; }
    renderStatus();
  }
  async function readList() {
    const target = state.requestedPage || { index: state.pageIndex, after: state.pageStack[state.pageIndex] };
    const params = new URLSearchParams({ limit: "25" });
    if (target.after !== null) params.set("after", target.after);
    try {
      const response = await get(`/api/tasks?${params}`);
      state.list = response;
      state.listError = "";
      state.pageStack[target.index] = target.after;
      state.pageIndex = target.index;
      if (state.requestedPage === target) state.requestedPage = null;
      if (!state.selected && array(response.result.tasks).length) selectTask(response.result.tasks[0].id);
    } catch (failure) {
      state.listError = failure.message;
      if (state.requestedPage === target) state.requestedPage = null;
    }
    renderList();
  }
  async function readDetail(id) {
    const cached = entry(id);
    const after = cached.nextAfter !== null ? cached.nextAfter : cached.lastSequence;
    const params = new URLSearchParams({ task_id: id, events_after: String(after), event_limit: "25" });
    try {
      const response = await get(`/api/task?${params}`);
      if (response.result.task.id !== id) throw new Error("返回的任务身份与请求不一致，未更新快照。");
      cached.response = response;
      cached.error = "";
      for (const event of array(response.result.events)) {
        cached.events.set(event.sequence, event);
        cached.lastSequence = Math.max(cached.lastSequence, event.sequence);
      }
      cached.nextAfter = response.result.events_next_after ?? null;
      cached.hasEventPage = true;
      cached.eventObservedAt = response.observed_at;
    } catch (failure) { cached.error = failure.message; }
    if (state.selected === id) renderDetail();
  }

  function renderStatus() {
    const target = $("status-data");
    if (state.status) {
      const result = state.status.result;
      target.replaceChildren(fields([
        ["版本", result.version], ["仓库", result.repository, true], ["Socket", result.socket, true]
      ]));
    }
    if (!state.status) target.textContent = "尚未取得控制器快照；请检查独立 daemon 与配置。";
    const label = state.statusError ? (state.status ? "读取失败 · 旧快照" : "连接失败") : (state.status ? "最近读取成功" : "尚未观测");
    $("connection-state").textContent = label;
    $("connection-state").className = `badge ${state.statusError ? "warning" : state.status ? "good" : "neutral"}`;
    freshness("status-freshness", state.status?.observed_at);
    error("status-error", state.statusError);
  }
  function renderList() {
    const result = state.list?.result;
    const tasks = array(result?.tasks);
    const visible = tasks.filter(task => state.filter === "all" || task.state === state.filter);
    const target = $("task-list");
    const focusedId = target.contains(document.activeElement) ? document.activeElement?.dataset.taskId : null;
    target.replaceChildren();
    for (const task of visible) {
      const button = element("button", "task-card");
      button.type = "button";
      button.dataset.taskId = task.id;
      button.setAttribute("aria-pressed", String(task.id === state.selected));
      const top = element("div", "task-card-top");
      const id = element("span", "task-short-id mono", task.id);
      id.title = task.id;
      top.append(id, badge(STATES[task.state] || task.state, task.state));
      button.append(top, element("h3", "", task.title), element("span", "branch mono", text(task.branch)));
      button.append(element("span", "task-meta", `${task.worker ? "Worker 有运行记录" : "Worker 未创建"} · ${task.reviewer ? "Reviewer 有运行记录" : "Reviewer 未创建"}`));
      button.append(element("span", "task-meta", `更新于 ${time(task.updated_at)}`));
      if (task.last_error_excerpt) button.append(element("span", "task-card-error", task.last_error_excerpt));
      button.addEventListener("click", () => selectTask(task.id));
      target.append(button);
      if (focusedId === task.id) button.focus({ preventScroll: true });
    }
    if (!visible.length) target.append(element("p", "empty", state.list ? (tasks.length ? "当前页没有匹配状态的任务。可切换状态或翻页；不代表全局没有此状态。" : "当前页没有任务。") : "尚未取得任务页。失败不会填入演示数据。"));
    $("page-summary").textContent = state.list ? `本页已读取 ${tasks.length} 项 · 筛选显示 ${visible.length} 项${state.requestedPage ? ` · 正在读取第 ${state.requestedPage.index + 1} 页（下方仍为旧页）` : ""}` : "尚未取得任务页；不是全局总量。";
    $("page-number").textContent = `第 ${state.pageIndex + 1} 页`;
    $("previous-page").disabled = !!state.requestedPage || state.pageIndex === 0;
    $("next-page").disabled = !!state.requestedPage || !result?.next_after;
    freshness("list-freshness", state.list?.observed_at, "当前页成功观测");
    error("list-error", state.listError);
    const blockers = $("integration-blockers");
    blockers.replaceChildren();
    if (!state.list) blockers.append(element("span", "muted", "尚未读取全局集合"));
    else if (!array(result.integration_blockers).length) blockers.append(element("span", "muted", "本次全局观测未报告集成占位 / 阻塞；不是全队无等待或 CI 全通过。"));
    else {
      blockers.append(element("span", "badge warning", `全局 ${result.integration_blockers.length} 项`));
      for (const id of result.integration_blockers) {
        const button = element("button", "", id);
        button.type = "button";
        button.addEventListener("click", () => selectTask(id));
        blockers.append(button);
      }
    }
    freshness("blockers-freshness", state.list?.observed_at, state.listError ? "全局集合上次成功观测（本轮读取失败）" : "全局集合成功观测");
    renderLocation();
  }
  function renderLocation() {
    if (!state.selected) { $("selection-location").textContent = "尚未选择任务"; return; }
    const onPage = array(state.list?.result.tasks).find(task => task.id === state.selected);
    $("selection-location").textContent = onPage ? ((state.filter === "all" || state.filter === onPage.state) ? "所选任务位于当前页" : "所选任务被当前页筛选隐藏 · 保留详情") : "所选任务不在当前页 · 保留详情";
  }
  function selectTask(id) {
    if (!id) return;
    state.selected = id;
    state.detailSignature = "";
    renderList();
    renderDetail();
    request(["detail"]);
  }

  function renderDetail() {
    const cached = entry(state.selected);
    const response = cached.response;
    error("detail-error", cached.error);
    freshness("detail-freshness", response?.observed_at, "详情成功观测");
    if (!response) {
      $("detail-title").textContent = state.selected;
      $("detail-content").replaceChildren(element("p", "empty", cached.error ? "暂未取得此任务的成功快照。任务选择已保留，后续轮询会继续读取。" : "正在读取任务详情与首批事件…"));
      return;
    }
    const task = response.result.task;
    $("detail-title").textContent = text(task.spec?.title, task.id);
    const observations = response.observations || {};
    const stableObservation = value => value ? { ...value, observed_at: undefined } : null;
    const signature = JSON.stringify({
      task, pending: [response.result.worker_pending, response.result.reviewer_pending],
      stale: !!cached.error,
      observations: [stableObservation(observations.worker), stableObservation(observations.reviewer)],
      events: [...cached.events.values()], nextAfter: cached.nextAfter, eventFilter: state.eventFilter
    });
    if (signature !== state.detailSignature) {
      const content = $("detail-content");
      const openStates = new Map([...content.querySelectorAll("details[data-disclosure]")].map(node => [node.dataset.disclosure, node.open]));
      const focusedControl = content.contains(document.activeElement) ? document.activeElement?.id : null;
      content.replaceChildren();
      const strip = element("div", "summary-strip");
      strip.append(badge(STATES[task.state] || task.state, task.state), element("span", "mono", task.id));
      content.append(strip);
      content.append(element("p", "section-note", "上方是控制器保存的任务阶段，不是进程健康状态，也不表示模型正在输出。下方进程观测、待答记录与历史事件分别核对。"));
      if (STAGE_NOTES[task.state]) content.append(notice(STAGE_NOTES[task.state]));
      if (task.last_error) content.append(element("p", "error", `任务最后错误记录：${task.last_error}`));
      content.append(buildSpec(task));
      const agents = section("运行实例与原生现场", "agents");
      agents.append(element("p", "section-note", "“运行中”仅表示服务端验证了私有 tmux pane、run ID 与实际 PID；不代表模型忙碌、没有待答或任务成功。记录中的 PID / 会话路径不是当前存活证据。"));
      const grid = element("div", "agent-grid");
      for (const role of ["worker", "reviewer"]) grid.append(buildAgent(role, task[role], observations[role], response.result[`${role}_pending`], !!cached.error));
      agents.append(grid, notice("原生权限确认弹窗不可观测。pending 为 null 只表示本次没有读到 ask 记录，不能推断没有等待；进程仍运行也可能正在等待用户。请回对应窗口核实，不要在网页代答。"));
      content.append(agents, buildDelivery(task, cached), buildFeedback(cached), buildEvents(cached));
      for (const disclosure of content.querySelectorAll("details[data-disclosure]")) {
        if (openStates.has(disclosure.dataset.disclosure)) disclosure.open = openStates.get(disclosure.dataset.disclosure);
      }
      if (focusedControl) $(focusedControl)?.focus({ preventScroll: true });
      state.detailSignature = signature;
    }
    for (const role of ["worker", "reviewer"]) {
      const target = $(`observation-time-${role}`);
      if (target) target.textContent = observations[role]?.observed_at ? `${cached.error ? "上次进程观测（本轮读取失败）" : "进程观测"}：${time(observations[role].observed_at)}` : "尚无进程观测时间";
    }
    const eventTime = $("event-observed-time");
    if (eventTime) eventTime.textContent = `事件页成功观测：${time(cached.eventObservedAt)}；后续变化由增量读取发现。`;
  }

  function buildSpec(task) {
    const spec = task.spec || {};
    const node = section("任务约定与关系", "spec", true, true);
    node.append(element("h4", "", "目标"), element("p", "text-block", text(spec.goal)));
    node.append(element("h4", "", "验收标准"), valuesList(spec.acceptance, "未记录验收标准"));
    node.append(element("h4", "", "非目标"), valuesList(spec.non_goals, "未列出非目标"));
    node.append(fields([
      ["request_id", spec.request_id, true], ["基线 SHA", spec.base_sha, true],
      ["branch_slug", spec.branch_slug, true], ["模型请求", spec.model ?? "未指定（由运行配置决定）"],
      ["任务分支", task.branch, true], ["任务 worktree", task.worktree, true],
      ["创建时间", timestamp(task.created_at)], ["记录更新时间", timestamp(task.updated_at)],
      ["修复哪个任务", selectLink(spec.repairs_task_id)], ["被哪个任务修复", selectLink(task.repaired_by)],
      ["修复承担原目标", spec.completes_original_goal ? "是（任务约定，不等于验收已通过）" : "否"],
      ["创建者 / 大管家", "未追踪具体身份或会话；request_id 不是创建者身份"],
      ["等待 / 依赖关系", "不维护任意任务依赖图；仅显示上述明确修复关联和下方待答证据"]
    ]));
    return node;
  }

  function buildAgent(role, run, observation, pending, stale) {
    const name = role === "worker" ? "Worker" : "Reviewer";
    const node = element("article", "agent-card");
    const heading = element("div", "section-heading");
    const observedState = observation?.state;
    const observedLabel = !run ? "尚未创建" : ({ running: "进程运行中", not_running: "未观测到运行", unknown: "进程未知" }[observedState] || "缺少进程观测");
    heading.append(element("h4", "", name), badge(`${stale ? "旧观测 · " : ""}${observedLabel}`, stale ? "unknown" : observedState || "unknown"));
    node.append(heading);
    if (!run) {
      node.append(element("p", "section-note", "此任务尚无该角色的运行记录，不创建虚构会话或在线计数。"));
      if (pending) node.append(notice("出现待答记录但没有对应运行实例；请核对原生现场，不能据此认定正在等待。"), element("pre", "", json(pending)));
      return node;
    }
    node.append(element("p", "section-note", observation ? text(observation.reason, "服务端未提供观测原因") : "本次响应没有提供可用进程观测；不依据任务状态或历史 PID 推断存活。"));
    const observedTime = element("p", "freshness");
    observedTime.id = `observation-time-${role}`;
    node.append(observedTime);
    node.append(fields([
      ["Run ID", run.id, true], ["记录角色", run.role], ["窗口 / pane", `${text(run.window)} / ${text(run.pane)}`, true],
      ["实际 pane PID", observation?.pane_pid, true], ["实际 Agent PID", observation?.agent_pid, true],
      ["历史记录 PID", run.pid, true], ["运行开始", timestamp(run.started_at)],
      ["会话文件", run.session_file ?? "尚未记录（初始会话也可能尚未落盘）", true]
    ]));
    const paths = element("details");
    paths.dataset.disclosure = `paths-${role}`;
    paths.append(element("summary", "", "完整工作区与持久目录"), fields([
      ["worktree", run.worktree, true], ["runtime_dir", run.runtime_dir, true], ["session_dir", run.session_dir, true]
    ]));
    node.append(paths);
    if (observation?.terminal_command) {
      const commands = element("div", "command-block");
      commands.append(element("p", "section-note", "原生接入命令（仅复制）。进入后按 Ctrl-b w 选择上方窗口；如已在 tmux，先脱离避免嵌套。"));
      commands.append(element("pre", "", observation.terminal_command));
      const copy = element("button", "", "复制接入命令");
      copy.type = "button";
      copy.id = `copy-${role}`;
      copy.addEventListener("click", () => copyCommand(observation.terminal_command));
      commands.append(copy);
      node.append(commands);
    } else node.append(element("p", "section-note", "没有可复制的安全观测命令；不使用旧版未引用 terminal_command。"));
    if (pending === null || pending === undefined) {
      node.append(element("p", "section-note", "待答：本次未读到 ask 记录；不是“没有等待”的证明。"));
    } else {
      const box = element("div", "pending-card");
      box.append(element("h4", "", "存在 ask 待答记录"));
      if (pending.run_id !== run.id) box.append(notice("待答记录 run_id 与当前运行记录不一致，可能是历史残留；不能当作当前等待证据。"));
      box.append(element("p", "", "记录可能早于弹窗或在崩溃后残留；去原生窗口核对与回答。待答标记被清理也不等于用户已回答。"));
      box.append(fields([["记录时间", timestamp(pending.at)], ["记录 Run ID", pending.run_id, true], ["工具调用 ID", pending.toolCallId, true]]));
      box.append(element("h4", "", "原始提问输入"), element("pre", "", json(pending.input)));
      const raw = element("details");
      raw.dataset.disclosure = `pending-${role}`;
      raw.append(element("summary", "", "完整待答记录"), element("pre", "", json(pending)));
      box.append(raw);
      node.append(box);
    }
    return node;
  }

  function buildDelivery(task, cached) {
    const node = section("PR · Review · CI 版本证据", "delivery", true, true);
    node.append(element("p", "section-note", "以下为 daemon 保存的证据，不是浏览器实时 GitHub 状态。SHA 保留完整值；不同版本不能互相代替，也不因一个 PASS 或某个进程退出而视为完成。"));
    if (task.pr) {
      const card = element("div", "evidence-card");
      card.append(element("h4", "", `Pull Request #${task.pr.number}`), fields([
        ["PR", safeLink(task.pr.url)], ["PR base SHA", task.pr.base_sha, true], ["PR head SHA", task.pr.head_sha, true]
      ]));
      node.append(card);
    } else node.append(element("p", "empty", "未记录 PR。"));
    node.append(fields([
      ["审查固定 base", task.review_base, true], ["审查固定 head", task.review_head, true],
      ["合并授权 head", task.merge_authorized_head, true], ["实际 merge SHA", task.merge_sha, true]
    ]));
    const prEvents = sortedEvents(cached).filter(event => /^pr_ci_/.test(event.kind));
    const ciHistory = element("details", "evidence-card");
    ciHistory.dataset.disclosure = "pr-ci-history";
    ciHistory.append(element("summary", "", `PR CI 版本证据 · 已加载 ${prEvents.length} 条`));
    ciHistory.append(element("p", "section-note", "只呈现已加载事件中的实际记录，不当作当前 head 的实时检查结果。base / head 缺失时不推断版本，也不补造通过证据。"));
    if (!prEvents.length) ciHistory.append(element("p", "empty", "已加载历史中未发现 PR CI 事件；可继续读取后续事件或查看 PR。"));
    for (const event of prEvents) {
      const evidence = element("div", "evidence-card");
      evidence.append(element("h4", "", `#${event.sequence} ${EVENT_NAMES[event.kind] || event.kind}`));
      evidence.append(fields([
        ["记录时间", timestamp(event.at)], ["事件 base SHA", event.data?.base_sha, true],
        ["事件 head SHA", event.data?.head_sha, true]
      ]));
      evidence.append(element("pre", "", json(event.data)));
      ciHistory.append(evidence);
    }
    node.append(ciHistory);
    if (task.review_report) {
      const report = task.review_report;
      const card = element("div", "evidence-card");
      const heading = element("div", "section-heading");
      heading.append(element("h4", "", "结构化审查报告"), badge(text(report.verdict), report.verdict));
      card.append(heading, fields([["报告 base SHA", report.base_sha, true], ["报告 head SHA", report.head_sha, true]]));
      card.append(element("p", "text-block", text(report.summary)), element("h4", "", "发现"), valuesList(report.findings, "报告未列出发现。"));
      card.append(element("h4", "", "报告记载的验证"), valuesList(report.verification, "报告未列出验证；看板不补造通过记录。"));
      node.append(card);
    } else node.append(element("p", "empty", "未记录结构化审查报告。"));
    node.append(element("h4", "", "合并后 CI 证据"));
    if (!array(task.post_ci).length) node.append(element("p", "empty", "未记录合并后 CI；不表示已通过，也不表示已运行。"));
    for (const ci of array(task.post_ci)) {
      const card = element("div", "evidence-card");
      const heading = element("div", "section-heading");
      heading.append(element("h4", "", text(ci.workflow)), badge(text(ci.conclusion), ci.conclusion));
      card.append(heading, fields([["Run ID", ci.run_id, true], ["检查 head SHA", ci.head_sha, true], ["运行链接", safeLink(ci.url)]]));
      node.append(card);
    }
    node.append(element("p", "section-note", "PR CI 的细节以已加载事件及 PR 链接为线索；此 API 不提供独立的实时 PR 检查矩阵。旧事件可能对应旧 head，需核对事件中的版本。"));
    return node;
  }

  function sortedEvents(cached) { return [...cached.events.values()].sort((a, b) => a.sequence - b.sequence); }
  function buildFeedback(cached) {
    const node = section("返工反馈与等待线索", "feedback", true, false);
    node.append(notice("worker_feedback_delivered 仅证明控制器把反馈写入持久 inbox；不证明 Worker 已读取、执行、完成修改或回答了用户。排队 / delivered 记录也不能代替原生问答结果。"));
    const events = sortedEvents(cached).filter(event => /feedback|inbox|waiting|ask_/.test(event.kind));
    node.append(element("p", "section-note", `仅检查已加载的 ${cached.events.size} 条事件，其中 ${events.length} 条是反馈 / 等待线索。${cached.nextAfter !== null ? "历史尚有后续，请继续读取；未显示不代表未发生。" : "本次事件页未报告更多后续；新事件仍需轮询发现。"}`));
    if (!events.length) node.append(element("p", "empty", "已加载片段没有反馈 / 等待事件；不能据此认定没有待答或没有反馈。"));
    else node.append(eventList(events, "feedback"));
    return node;
  }
  function buildEvents(cached) {
    const node = section("事件时间线", "events");
    const more = cached.nextAfter !== null;
    node.append(element("p", "section-note", `已保留 ${cached.events.size} 条去重事件，按 sequence 升序。${more ? `历史尚有后续，服务端续页游标 ${cached.nextAfter}；每轮继续读取至多 25 条。` : "当前已读到本次快照尾部；不代表任务不会再产生事件。"} 切换任务或任务分页不清空已读历史。`));
    const controls = element("div", "event-controls");
    const filter = element("select");
    filter.id = "event-filter";
    filter.setAttribute("aria-label", "筛选已加载事件");
    for (const [value, label] of [["all", "已加载 · 全部事件"], ["waiting", "已加载 · 问答 / 等待"], ["feedback", "已加载 · 反馈"], ["delivery", "已加载 · 交付 / CI / 审查"]]) {
      const option = element("option", "", label);
      option.value = value;
      filter.append(option);
    }
    filter.value = state.eventFilter;
    filter.addEventListener("change", () => { state.eventFilter = filter.value; renderDetail(); });
    const load = element("button", "", more ? "继续读取后续事件" : "检查新事件");
    load.id = "load-events";
    load.type = "button";
    load.addEventListener("click", () => request(["detail"]));
    controls.append(filter, load);
    node.append(controls);
    const match = {
      all: () => true, waiting: event => /ask_|waiting|pending/.test(event.kind),
      feedback: event => /feedback|inbox/.test(event.kind),
      delivery: event => /ci|review|pr_|merge|integration|delivery|completed|repair/.test(event.kind)
    };
    const events = sortedEvents(cached).filter(match[state.eventFilter] || match.all);
    node.append(element("p", "section-note", `当前筛选显示 ${events.length} / ${cached.events.size} 条已加载事件；并非全局事件数量。`));
    if (events.length) node.append(eventList(events, "timeline"));
    else node.append(element("p", "empty", "已加载片段中没有匹配事件。"));
    const at = element("p", "freshness");
    at.id = "event-observed-time";
    node.append(at);
    return node;
  }
  function eventList(events, prefix) {
    const list = element("ol", "event-list");
    for (const event of events) {
      const item = element("li", "event-item");
      const disclosure = element("details");
      disclosure.dataset.disclosure = `${prefix}-${event.sequence}`;
      const summary = element("summary");
      summary.append(element("span", "event-label", `#${event.sequence} ${EVENT_NAMES[event.kind] || event.kind}`));
      summary.append(element("span", "event-kind mono", event.kind));
      disclosure.append(summary, element("p", "event-time", `${time(event.at)} · ${text(event.task_id)}`));
      if (event.kind === "worker_feedback_delivered") disclosure.append(element("p", "section-note", "仅写入 inbox；不是 Worker 已消费、执行或完成的证据。"));
      if (event.kind === "ask_cleared" || event.kind === "ask_result") disclosure.append(element("p", "section-note", "工具返回 / 标记清理可能是失败、结束或重定向，不等同于人类回答。"));
      disclosure.append(element("pre", "", json(event.data)));
      item.append(disclosure);
      list.append(item);
    }
    return list;
  }
  async function copyCommand(command) {
    clearTimeout(state.copyTimer);
    try {
      if (!navigator.clipboard?.writeText) throw new Error("clipboard unavailable");
      await navigator.clipboard.writeText(command);
      $("copy-status").textContent = "已复制接入命令；网页没有执行它。请自行在终端核对后使用。";
    } catch {
      $("copy-status").textContent = "浏览器未允许剪贴板写入。请手动选择上方完整命令复制；网页不会执行命令。";
    }
    state.copyTimer = setTimeout(() => { $("copy-status").textContent = ""; }, 6500);
  }
  function navigate(direction) {
    if (state.requestedPage || !state.list) return;
    const index = state.pageIndex + direction;
    if (index < 0) return;
    const after = direction > 0 ? state.list.result.next_after : state.pageStack[index];
    if (direction > 0 && !after) return;
    state.requestedPage = { index, after };
    renderList();
    request(["list"]);
  }

  for (const [value, label] of Object.entries(STATES)) {
    const option = element("option", "", `当前页 · ${label}`);
    option.value = value;
    $("state-filter").append(option);
  }
  $("state-filter").addEventListener("change", event => { state.filter = event.target.value; renderList(); });
  $("previous-page").addEventListener("click", () => navigate(-1));
  $("next-page").addEventListener("click", () => navigate(1));
  $("refresh").addEventListener("click", () => request(state.selected ? ["status", "list", "detail"] : ["status", "list"]));
  request(["status", "list"]);
})();
