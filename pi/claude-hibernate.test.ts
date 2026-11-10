// Run with `node --test pi/claude-hibernate.test.ts` (node 23.6+ runs TypeScript).
// Drives the extension with a fake pi and reads the state file it writes.
import { test } from "node:test";
import assert from "node:assert/strict";
import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";

const dir = fs.mkdtempSync(path.join(os.tmpdir(), "ch-pi-ext-"));
const statePath = path.join(dir, "state");
process.env.CLAUDE_HIBERNATE_PI_STATE = statePath;
const { default: extension } = await import("./claude-hibernate.ts");

type Handler = (e: any, ctx: any) => unknown;
const handlers = new Map<string, Handler[]>();
const bus = new Map<string, ((d: unknown) => void)[]>();
const entries: any[] = [];
const pi = {
  on: (name: string, h: Handler) => handlers.set(name, [...(handlers.get(name) ?? []), h]),
  events: {
    on: (ch: string, h: (d: unknown) => void) => bus.set(ch, [...(bus.get(ch) ?? []), h]),
    emit: (ch: string, d: unknown) => bus.get(ch)?.forEach((h) => h(d)),
  },
};
const ctx = {
  hasUI: true,
  ui: { getEditorText: () => "", setEditorText: () => {} },
  hasPendingMessages: () => false,
  isIdle: () => true,
  sessionManager: {
    getSessionId: () => "s1", getSessionFile: () => "/tmp/s1.jsonl", getCwd: () => "/tmp",
    getSessionName: () => undefined, getEntries: () => entries,
  },
};
const fire = (name: string, e: any = {}) => handlers.get(name)?.forEach((h) => h(e, ctx));
const work = () => JSON.parse(fs.readFileSync(statePath, "utf8")).work as string[];
const job = (id: string, status: string) =>
  pi.events.emit("aria-local:background-task-update", { task: { task_id: id, label: "sleep 300", status } });

extension(pi);
fire("session_start", { reason: "startup" });

test("a tool call is work until it ends", () => {
  fire("tool_execution_start", { toolCallId: "c1", toolName: "bash" });
  assert.deepEqual(work(), ["tool bash"]);
  fire("tool_execution_end", { toolCallId: "c1", toolName: "bash" });
  assert.deepEqual(work(), []);
});

test("a bash-background job is work while it runs", () => {
  job("t1", "running");
  assert.deepEqual(work(), ["background job sleep 300"]);
  job("t1", "completed");
  assert.deepEqual(work(), []);
});

test("a ! command is work until pi records its result", () => {
  fire("user_bash", { command: "make test" });
  assert.deepEqual(work(), ["! make test"]);
  entries.push({ type: "message", message: { role: "bashExecution", command: "make test", timestamp: Date.now() + 1 } });
  // The extension's one-second poll finds the result.
  return new Promise<void>((done) => setTimeout(() => {
    assert.deepEqual(work(), []);
    done();
  }, 1200));
});

test("shutdown drops jobs bash-background kills without a final update", () => {
  job("t2", "running");
  assert.equal(work().length, 1);
  fire("session_shutdown", { reason: "quit" });
  assert.deepEqual(work(), []);
});
