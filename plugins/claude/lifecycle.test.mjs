import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import { spawn } from "node:child_process";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync, copyFileSync } from "node:fs";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";
import { fileURLToPath } from "node:url";
import { initialize, handleHookEvent, resetClientForTesting, cachedTaskInjection } from "./dist/index.js";

const here = fileURLToPath(new URL(".", import.meta.url));
const approvedTemp = process.platform === "win32"
  ? join(process.env.LOCALAPPDATA, "Temp", "opencode") : tmpdir();
const installer = readFileSync(new URL("../../src/task/harness_plugins.rs", import.meta.url), "utf8");
const installedRunner = installer.match(/const CLI_HOOK_RUNNER_MJS: &str = r#"([\s\S]*?)"#;/)[1];

async function fixture(runner, retirement = "superseded") {
  const root = mkdtempSync(join(approvedTemp, "claude-freshness-"));
  const nsDir = join(root, ".NeuroStrata");
  const adapter = join(nsDir, "plugins", "claude");
  mkdirSync(join(adapter, "dist"), { recursive: true });
  mkdirSync(join(root, "home"), { recursive: true });
  copyFileSync(join(here, "dist/index.js"), join(adapter, "dist/index.js"));
  writeFileSync(join(adapter, "package.json"), '{"type":"module"}');
  writeFileSync(join(adapter, "cli-hook.mjs"), runner);
  writeFileSync(join(root, "empty.token"), "");
  const sessionPath = join(nsDir, "active_session.json");
  const old = {
    namespace: "example-project", task_id: "task-current", status: "in_progress",
    context_revision: "claim-time", subagent_prompt_injection: `RETIRED_CLAIM_PROSE_${retirement}`,
    golden_rules: ["RETIRED_GOLDEN"], conceptual_bridges: ["RETIRED_BRIDGE"],
    host_key: { preserve: true },
  };
  writeFileSync(sessionPath, JSON.stringify(old));
  let revision = 0;
  let persist = true;
  let unavailable = false;
  const calls = [];
  const server = http.createServer((req, res) => {
    let body = "";
    req.on("data", chunk => body += chunk);
    req.on("end", () => {
      if (unavailable) { res.writeHead(503); res.end(); return; }
      const rpc = JSON.parse(body);
      let text = "{}";
      if (rpc.method === "tools/call") {
        const { name, arguments: args } = rpc.params;
        calls.push({ name, args });
        if (name === "neurostrata_get_snapshot" && persist) {
          assert.equal(resolve(args.project_root), resolve(root));
          assert.equal(args.namespace, "example-project");
          const current = JSON.parse(readFileSync(sessionPath, "utf8"));
          writeFileSync(sessionPath, JSON.stringify({ ...current,
            context_revision: `fresh-${++revision}`,
            golden_rules: ["LIVE_CURRENT_RULE; unaffected exception retained"],
            subagent_prompt_injection: "CURRENT_SUCCESSOR src/example.rs:10-20 full anchor",
            conceptual_bridges: ["LIVE_CURRENT_BRIDGE"],
          }));
        }
        if (name === "neurostrata_task_list") text = JSON.stringify({ tasks: [{
          id: "task-current", status: "in_progress", session_id: "claim-session",
          subagent_prompt_injection: "RETIRED_CLAIM_PROSE",
        }] });
        if (name === "neurostrata_memories_for_file") text = JSON.stringify({ memories: [{
          id: "live-file-rule", memory_type: "rule", content: "LIVE_FILE_RULE; unaffected clause",
        }] });
      }
      res.writeHead(200, { "Content-Type": "application/json", Connection: "close" });
      res.end(JSON.stringify({ jsonrpc: "2.0", id: rpc.id,
        result: rpc.method === "tools/list" ? { tools: [
          { name: "neurostrata_guard_validate" }, { name: "neurostrata_task_create" },
        ] } : { content: [{ type: "text", text }] } }));
    });
  });
  await new Promise(resolve => server.listen(0, "127.0.0.1", resolve));
  const config = { namespace: "example-project",
    endpoint: `http://127.0.0.1:${server.address().port}/mcp`,
    tokenFile: join(root, "empty.token"), requestTimeoutMs: 200 };
  const saveConfig = () => writeFileSync(join(nsDir, "neurostrata-plugin.json"), JSON.stringify(config));
  saveConfig();
  async function run(event) {
    return await new Promise((resolve, reject) => {
      const child = spawn(process.execPath, [join(adapter, "cli-hook.mjs")], {
        cwd: root, env: { ...process.env, HOME: join(root, "home"),
          NEUROSTRATA_TOKEN_DIR: join(root, "empty.token"),
          NEUROSTRATA_NAMESPACE: "example-project" }, stdio: ["pipe", "pipe", "pipe"],
      });
      let stdout = "", stderr = "";
      child.stdout.on("data", b => stdout += b);
      child.stderr.on("data", b => stderr += b);
      child.on("error", reject);
      child.on("close", code => {
        try { assert.equal(code, 0, stderr); resolve(JSON.parse(stdout)); } catch (e) { reject(e); }
      });
      child.stdin.end(JSON.stringify({ session_id: "host-session", cwd: root, ...event }));
    });
  }
  return { root, calls, run, sessionPath, config, offline() {
    config.endpoint = "http://127.0.0.1:1/mcp"; saveConfig();
  }, unchanged() { persist = false; }, unavailable() { unavailable = true; },
  close: () => new Promise(resolve => server.close(resolve)) };
}

for (const [label, runner] of [
  ["installer", installedRunner],
  ["adapter", readFileSync(join(here, "cli-hook.mjs"), "utf8")],
]) {
  for (const retirement of ["archived", "superseded"]) {
  test(`${label} CLI ${retirement} projection: SessionStart startup/resume/compact refreshes baseline; offline omits stale prose`, async () => {
    const f = await fixture(runner, retirement);
    try {
      for (const source of ["startup", "resume", "compact"]) {
        const out = await f.run({ hook_event_name: "SessionStart", source, additionalContext: "HOST_CONTEXT" });
        assert.equal(out.hookSpecificOutput.hookEventName, "SessionStart");
        assert.equal(out.hookSpecificOutput.permissionDecision, undefined);
        assert.match(out.hookSpecificOutput.additionalContext, /HOST_CONTEXT/);
        assert.match(out.hookSpecificOutput.additionalContext, /LIVE_CURRENT_RULE; unaffected exception retained/);
        assert.match(out.hookSpecificOutput.additionalContext, /CURRENT_SUCCESSOR src\/example.rs:10-20 full anchor/);
        assert.match(out.hookSpecificOutput.additionalContext, /LIVE_CURRENT_BRIDGE/);
        assert.doesNotMatch(JSON.stringify(out), /RETIRED_/);
      }
      assert.equal(f.calls.filter(c => c.name === "neurostrata_get_snapshot").length, 3);
      assert.deepEqual(JSON.parse(readFileSync(f.sessionPath)).host_key, { preserve: true });
      const precompact = await f.run({ hook_event_name: "PreCompact", trigger: "auto" });
      assert.equal(precompact.hookSpecificOutput?.additionalContext, undefined);
      assert.equal(f.calls.filter(c => c.name === "neurostrata_get_snapshot").length, 3);
      f.unchanged();
      const unchanged = await f.run({ hook_event_name: "SessionStart", source: "compact" });
      assert.equal(unchanged.hookSpecificOutput?.additionalContext, undefined);
      f.offline();
      const offline = await f.run({ hook_event_name: "SessionStart", source: "compact", additionalContext: "HOST_ONLY" });
      assert.equal(offline.hookSpecificOutput.additionalContext, "HOST_ONLY");
      assert.doesNotMatch(JSON.stringify(offline), /CURRENT_SUCCESSOR|RETIRED_|LIVE_CURRENT/);
    } finally { await f.close(); }
  });
  }

  test(`${label} CLI subagent never replays task_list claim-time prose; file touch remains current`, async () => {
    const f = await fixture(runner);
    try {
      const out = await f.run({ hook_event_name: "PreToolUse", tool_name: "Task",
        tool_input: { prompt: "Inspect src/example.rs; preserve this host instruction" } });
      const prompt = out.hookSpecificOutput.updatedInput.prompt;
      assert.match(prompt, /preserve this host instruction/);
      assert.match(prompt, /CURRENT_SUCCESSOR/);
      assert.match(prompt, /LIVE_FILE_RULE/);
      assert.doesNotMatch(prompt, /RETIRED_/);
      const read = await f.run({ hook_event_name: "PreToolUse", tool_name: "Read",
        tool_input: { file_path: "src/example.rs" } });
      assert.match(read.hookSpecificOutput.additionalContext, /LIVE_FILE_RULE; unaffected clause/);
      assert.doesNotMatch(JSON.stringify(read), /RETIRED_/);
      f.offline();
      const offline = await f.run({ hook_event_name: "PreToolUse", tool_name: "Task",
        tool_input: { prompt: "Inspect src/example.rs" } });
      assert.doesNotMatch(offline.hookSpecificOutput.updatedInput.prompt, /CURRENT_SUCCESSOR|RETIRED_|LIVE_FILE_RULE/);
    } finally { await f.close(); }
  });
}

test("long-lived Claude dispatch refreshes compact epoch and subagent cache, then fails soft offline", async () => {
  const f = await fixture(installedRunner);
  try {
    await initialize({ directory: f.root, config: f.config });
    for (const source of ["startup", "compact"]) {
      const out = await handleHookEvent({ hook_event_name: "SessionStart", source,
        session_id: "same-host-session", additionalContext: "HOST" });
      assert.match(out.hookSpecificOutput.additionalContext, /CURRENT_SUCCESSOR/);
    }
    assert.equal(f.calls.filter(c => c.name === "neurostrata_get_snapshot").length, 2);
    cachedTaskInjection.set("task-current", "RETIRED_CACHED_INJECTION");
    const child = await handleHookEvent({ hook_event_name: "PreToolUse", tool_name: "Task",
      tool_input: { prompt: "Inspect src/example.rs" } });
    assert.match(child.hookSpecificOutput.updatedInput.prompt, /CURRENT_SUCCESSOR/);
    assert.doesNotMatch(JSON.stringify(child), /RETIRED_/);
    f.unavailable();
    cachedTaskInjection.set("task-current", "RETIRED_CACHED_INJECTION");
    const offlineChild = await handleHookEvent({ hook_event_name: "PreToolUse", tool_name: "Task",
      tool_input: { prompt: "Inspect src/example.rs" } });
    assert.doesNotMatch(offlineChild.hookSpecificOutput.updatedInput.prompt,
      /CURRENT_SUCCESSOR|RETIRED_|LIVE_FILE_RULE/);
    f.offline();
    await initialize({ directory: f.root, config: f.config });
    const offline = await handleHookEvent({ hook_event_name: "SessionStart", source: "compact",
      session_id: "same-host-session", additionalContext: "HOST_ONLY" });
    assert.equal(offline.hookSpecificOutput.additionalContext, "HOST_ONLY");
  } finally { resetClientForTesting(); await f.close(); }
});
