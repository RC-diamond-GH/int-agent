import type { ExtensionAPI, ExtensionContext } from "@oh-my-pi/pi-coding-agent";
import * as fs from "node:fs";
import * as path from "node:path";
import { spawnSync } from "node:child_process";

// Loaded only by a controller-owned launch. This is observation and message
// delivery, never an approval proxy or an answer to a native ask dialog.
export default function (pi: ExtensionAPI) {
  const runId = process.env.INT_AGENT_RUN_ID;
  const directory = process.env.INT_AGENT_RUNTIME_DIR;
  const inbox = process.env.INT_AGENT_INBOX;
  if (!runId || !directory || !inbox) return;
  let context: ExtensionContext | undefined;
  let pending: string | undefined;
  let inFlight: string | undefined;
  let polling = false;
  const syncDirectory = (directory: string) => {
    const fd = fs.openSync(directory, "r");
    try { fs.fsyncSync(fd); } finally { fs.closeSync(fd); }
  };
  const audit = (kind: string, data: unknown) => {
    const fd = fs.openSync(path.join(directory, "observer-events.jsonl"), "a", 0o600);
    try {
      fs.writeSync(fd, JSON.stringify({ at: new Date().toISOString(), run_id: runId, kind, data }) + "\n");
      fs.fsyncSync(fd);
    } finally { fs.closeSync(fd); }
    syncDirectory(directory);
  };
  const atomic = (file: string, data: unknown) => {
    const target = path.join(directory, file);
    const temporary = `${target}.${process.pid}.tmp`;
    const fd = fs.openSync(temporary, "w", 0o600);
    try { fs.writeSync(fd, JSON.stringify(data)); fs.fsyncSync(fd); }
    finally { fs.closeSync(fd); }
    fs.renameSync(temporary, target);
    syncDirectory(directory);
  };
  const title = (asking: boolean) => {
    const tmux = process.env.INT_AGENT_TMUX;
    const socket = process.env.INT_AGENT_TMUX_SOCKET;
    const pane = process.env.TMUX_PANE;
    const window = process.env.INT_AGENT_WINDOW;
    if (!tmux || !socket || !pane || !window) return;
    const result = spawnSync(tmux, ["-S", socket, "set-window-option", "-t", pane, "@int_agent_waiting", asking ? "1" : "0"], { timeout: 2000, maxBuffer: 65536 });
    if (result.status !== 0) audit("window_marker_error", { error: String(result.error ?? result.stderr) });
    if (asking) {
      const notice = spawnSync(tmux, ["-S", socket, "display-message", "-t", pane, "-d", "12000", `ASK requested in ${window}; Ctrl-b w to inspect`], { timeout: 2000, maxBuffer: 65536 });
      if (notice.status !== 0) audit("notification_error", { error: String(notice.error ?? notice.stderr) });
    }
  };
  const metadata = (ctx: ExtensionContext, ready: boolean) => atomic("observer.json", {
    run_id: runId, pid: process.pid, sessionFile: ctx.sessionManager.getSessionFile(),
    cwd: ctx.cwd, ready, at: new Date().toISOString(),
  });
  const clearPending = (reason: string) => {
    if (pending) audit("ask_cleared", { toolCallId: pending, reason });
    pending = undefined;
    fs.rmSync(path.join(directory, "pending.json"), { force: true });
    syncDirectory(directory);
    title(false);
  };
  const reportError = (ctx: ExtensionContext | undefined, error: unknown) => {
    const message = `int-agent observer: ${String(error)}`;
    pi.logger.warn(message);
    if (ctx?.hasUI) ctx.ui.notify(message, "warning");
  };
  const observe = (ctx: ExtensionContext, action: () => void) => {
    try { action(); } catch (error) { reportError(ctx, error); }
  };
  const poll = () => {
    const ctx = context;
    if (!ctx || polling || pending || inFlight || !ctx.isIdle() || ctx.hasPendingMessages()) return;
    polling = true;
    try {
      const files = fs.readdirSync(inbox).filter(name => name.endsWith(".json")).sort();
      for (const file of files) {
        if (fs.existsSync(path.join(inbox, file + ".ack"))) continue;
        const message = JSON.parse(fs.readFileSync(path.join(inbox, file), "utf8"));
        if (typeof message.id !== "string" || typeof message.text !== "string") throw new Error("Invalid durable inbox entry");
        inFlight = file;
        audit("inbox_delivery", { id: message.id, file });
        pi.appendEntry("int-agent-inbox", { id: message.id, run_id: runId });
        pi.sendUserMessage(`[Controller feedback ${message.id}]\n${message.text}`, { deliverAs: "followUp" });
        break;
      }
    } catch (error) {
      inFlight = undefined;
      reportError(ctx, error);
    }
    finally { polling = false; }
  };
  pi.on("session_start", async (_event, ctx) => {
    if (ctx.agent.kind !== "main") return;
    context = ctx;
    fs.mkdirSync(inbox, { recursive: true, mode: 0o700 });
    // Never expose the steward's recursive team tools, regardless of MCP server name.
    await pi.setActiveTools(pi.getActiveTools().filter(name => !/(^|[._:/-])team_(start_task|list_tasks|get_task|cancel_task|resume_task|authorize_merge|archive_task)$/.test(name)));
    observe(ctx, () => {
      clearPending("session_start");
      audit("session_start", { pid: process.pid, sessionFile: ctx.sessionManager.getSessionFile(), cwd: ctx.cwd });
      metadata(ctx, true);
    });
    ctx.setInterval(poll, 1000);
  });
  pi.on("tool_call", (event, ctx) => {
    if (ctx.agent.kind !== "main") return;
    if (/(^|[._:/-])team_(start_task|list_tasks|get_task|cancel_task|resume_task|authorize_merge|archive_task)$/.test(event.toolName)) {
      observe(ctx, () => audit("recursive_team_tool_blocked", { tool: event.toolName }));
      return { block: true, reason: "Workers and Reviewers must not invoke steward/team MCP tools." };
    }
    if (event.toolName !== "ask") return;
    observe(ctx, () => {
      pending = event.toolCallId;
      const data = { run_id: runId, at: new Date().toISOString(), toolCallId: event.toolCallId, input: event.input, sessionFile: ctx.sessionManager.getSessionFile(), cwd: ctx.cwd };
      atomic("pending.json", data);
      audit("ask_pending", data);
      title(true);
    });
  });
  pi.on("tool_result", (event, ctx) => {
    if (ctx.agent.kind !== "main" || event.toolName !== "ask") return;
    observe(ctx, () => {
      audit("ask_result", { toolCallId: event.toolCallId, isError: event.isError, content: event.content });
      // Completion can also mean chat redirect or an input error, not an answer.
      if (pending === event.toolCallId) clearPending(event.isError ? "failed" : "finished");
      metadata(ctx, true);
    });
  });
  pi.on("agent_end", (_event, ctx) => {
    if (ctx.agent.kind !== "main") return;
    observe(ctx, () => {
      clearPending("agent_end");
      metadata(ctx, true);
      if (inFlight) {
        const fd = fs.openSync(path.join(inbox, inFlight + ".ack"), "w", 0o600);
        try { fs.writeSync(fd, JSON.stringify({ run_id: runId, at: new Date().toISOString() })); fs.fsyncSync(fd); }
        finally { fs.closeSync(fd); }
        syncDirectory(inbox);
        audit("inbox_ack", { file: inFlight });
        inFlight = undefined;
      }
    });
  });
  pi.on("session_switch", (_event, ctx) => {
    if (ctx.agent.kind === "main") { context = ctx; observe(ctx, () => metadata(ctx, true)); }
  });
  pi.on("session_shutdown", (_event, ctx) => {
    if (ctx.agent.kind !== "main") return;
    observe(ctx, () => {
      clearPending("shutdown");
      metadata(ctx, false);
      audit("session_shutdown", {});
    });
    context = undefined;
  });
}
