// claude-hibernate's pi extension: reports this pi session's state to the
// wrapper that runs it, so the wrapper knows which session to resume and
// never puts pi to sleep while it is working or holding something unsent.
//
// The wrapper loads it with `-e` and names the state file in
// CLAUDE_HIBERNATE_PI_STATE. Without that variable it does nothing, so it is
// harmless when loaded any other way.
import * as fs from "node:fs";

const STATE = process.env.CLAUDE_HIBERNATE_PI_STATE;

interface State {
  pid: number;
  /** session_start events seen by this pi process (startup, /new, /resume, reload). */
  starts: number;
  session_id?: string;
  session_file?: string;
  cwd?: string;
  title?: string;
  /** An agent run is in progress (from agent_start until agent_settled). */
  busy: boolean;
  /** Messages queued for the agent (steering / follow-ups). */
  pending: boolean;
  /** Extension dialogs waiting on the user. */
  dialogs: number;
  /** Text sitting unsent in the editor. */
  draft: boolean;
  compacting: boolean;
  /** Last prompt the user submitted, and last agent activity (epoch seconds). */
  last_submit: number;
  last_event: number;
  /** When this pi process first ran a tool or a `!` command: children it
   *  started from then on may be work, not MCP servers (epoch seconds). */
  first_work: number;
}

// Session replacement (/new, /resume) and reload load a fresh copy of this
// module in the same process; keep one state across them.
const g = globalThis as any;
const state: State = (g.__claudeHibernatePi ??= {
  pid: process.pid, starts: 0, busy: false, pending: false, dialogs: 0,
  draft: false, compacting: false, last_submit: 0, last_event: 0, first_work: 0,
});

const now = () => Date.now() / 1000;
let written = "";
function write() {
  if (!STATE) return;
  const json = JSON.stringify(state);
  if (json === written) return;
  try {
    const tmp = `${STATE}.tmp${process.pid}`;
    fs.writeFileSync(tmp, json);
    fs.renameSync(tmp, STATE);
    written = json;
  } catch {
    // The wrapper is gone or its runtime dir was removed; nothing to report to.
  }
}

function text(content: unknown): string {
  if (typeof content === "string") return content;
  if (Array.isArray(content)) {
    return content.map((c: any) => (c?.type === "text" ? c.text : "")).join(" ");
  }
  return "";
}

/** The session name, or else its first prompt, as the sleep screen title. */
function title(ctx: any): string | undefined {
  try {
    const name = ctx.sessionManager.getSessionName?.();
    if (name) return name;
    for (const e of ctx.sessionManager.getEntries()) {
      if (e?.type === "message" && e.message?.role === "user") {
        const t = text(e.message.content).replace(/\s+/g, " ").trim();
        if (t) return t.slice(0, 80);
      }
    }
  } catch {}
  return undefined;
}

function fromContext(ctx: any) {
  try {
    const sm = ctx.sessionManager;
    state.session_id = sm.getSessionId();
    state.session_file = sm.getSessionFile();
    state.cwd = sm.getCwd?.() ?? ctx.cwd;
  } catch {}
  state.title = title(ctx) ?? state.title;
}

export default function (pi: any) {
  if (!STATE) return;
  let ctxNow: any;
  let timer: ReturnType<typeof setInterval> | undefined;

  // Things only visible by asking: the editor, the queue, idleness.
  const poll = () => {
    const ctx = ctxNow;
    if (!ctx) return;
    try {
      state.draft = ctx.hasUI ? ctx.ui.getEditorText().trim().length > 0 : false;
      state.pending = ctx.hasPendingMessages();
      if (ctx.isIdle() === false) state.busy = true;
    } catch {
      return; // stale context during a session switch
    }
    write();
  };

  pi.on("session_start", (_e: any, ctx: any) => {
    ctxNow = ctx;
    state.starts += 1;
    state.busy = false;
    state.dialogs = 0;
    state.compacting = false;
    state.last_event = now();
    fromContext(ctx);
    write();
    if (timer) clearInterval(timer);
    timer = setInterval(poll, 1000);
    timer.unref?.();
  });
  pi.on("session_shutdown", () => {
    ctxNow = undefined;
    if (timer) clearInterval(timer);
    timer = undefined;
  });
  pi.on("session_info_changed", (_e: any, ctx: any) => {
    fromContext(ctx);
    write();
  });
  pi.on("input", (e: any) => {
    if (e?.source !== "extension") state.last_submit = now();
    state.last_event = now();
    write();
  });
  pi.on("agent_start", () => {
    state.busy = true;
    state.last_event = now();
    write();
  });
  pi.on("agent_settled", (_e: any, ctx: any) => {
    state.busy = false;
    state.last_event = now();
    fromContext(ctx);
    write();
  });
  const work = () => {
    if (state.first_work === 0) {
      state.first_work = now();
      write();
    }
  };
  pi.on("tool_execution_start", work);
  pi.on("user_bash", () => {
    work();
    state.last_event = now();
    write();
  });
  pi.on("ui_prompt_start", () => {
    state.dialogs += 1;
    write();
  });
  pi.on("ui_prompt_end", () => {
    state.dialogs = Math.max(0, state.dialogs - 1);
    write();
  });
  pi.on("session_before_compact", () => {
    state.compacting = true;
    write();
  });
  const compacted = () => {
    state.compacting = false;
    state.last_event = now();
    write();
  };
  pi.on("session_compact", compacted);
  pi.on("session_compact_failed", compacted);
}
