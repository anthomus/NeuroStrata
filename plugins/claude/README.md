# @neurostrata/claude-plugin

NeuroStrata Claude Code CLI plugin. Provides Seamless Lifecycle + Guard validation,
file-touch memory prefetch, subagent prompt injection, and Zero-Action Start
enforcement inside Claude Code hook events.

## Install

```
npm install @neurostrata/claude-plugin
```

## MCP configuration

Add the NeuroStrata daemon to your Claude Code MCP config
(`~/.config/claude/mcp.json` or project-level `.mcp.json`):

```json
{
  "mcpServers": {
    "neurostrata": {
      "command": "node",
      "args": ["node_modules/@neurostrata/claude-plugin/dist/index.js"],
      "env": {
        "NEUROSTRATA_ENDPOINT": "http://127.0.0.1:34343/mcp",
        "NEUROSTRATA_TOKEN": "<daemon bearer token>",
        "NEUROSTRATA_NAMESPACE": "NeuroStrata"
      }
    }
  }
}
```

## Hook configuration (`.claude/settings.json`)

Register the plugin on Claude Code hook events:

```json
{
  "hooks": {
    "SessionStart": [
      {
        "hooks": [{
          "type": "command",
          "command": "node /path/to/@neurostrata/claude-plugin/cli-hook.mjs",
          "timeout": 30
        }]
      }
    ],
    "PreToolUse": [
      {
        "matcher": "Read|Write|Edit|MultiEdit|Bash|Task",
        "hooks": [{
          "type": "command",
          "command": "node /path/to/@neurostrata/claude-plugin/cli-hook.mjs",
          "timeout": 30
        }]
      }
    ],
    "Stop": [
      {
        "hooks": [{
          "type": "command",
          "command": "node /path/to/@neurostrata/claude-plugin/cli-hook.mjs",
          "timeout": 30
        }]
      }
    ]
  }
}
```

When invoked as a command hook, Claude Code passes the hook event as JSON on
stdin. The plugin reads it, processes it, and returns JSON on stdout with
`hookSpecificOutput`.

## Behavior by hook event

### SessionStart

Refreshes the snapshot before injecting the current task/rules baseline through
`hookSpecificOutput.additionalContext`. Includes startup, resume and
`source: "compact"` (after compaction); no matcher is needed. `PreCompact` does
not inject prose that the host could discard during compaction.

Requires a newly persisted, matching-namespace `context_revision`; unavailable
refresh, unchanged persistence or older daemons omit cached recall without
blocking the session. Host-provided context remains intact. Subagent task context
is refreshed too: claim-time prose from `task_list` is not replayed.

Contract: [Claude Code hooks reference](https://code.claude.com/docs/en/hooks#sessionstart).
The installer registers the same CLI runner on `SessionStart`; existing custom
runner commands remain user-owned. This does not upgrade an installed custom copy.

### PreToolUse

- **Task / subagent** tool calls — injects the subagent context header
  (`SUBAGENT_CONTEXT_MARKER`) into `tool_input.prompt` and returns
  `{ permissionDecision: "allow", updatedInput }`.
- **Read** tool calls — runs `prefetchMemoriesForFile` on the touched file
  (fail-soft, never blocks the tool).
- **Write / Edit / MultiEdit / Bash** — runs `neurostrata_guard_validate`
  when `strictMode` is true. On rejection returns
  `{ permissionDecision: "deny", permissionDecisionReason }`; otherwise
  increments the mutation counter and allows.

### Stop / SubagentStop

Returns `{ permissionDecision: "allow" }` — a no-op acknowledge.

## Build

```
cd plugins/claude
npm install
npm run build   # tsc → dist/index.js + dist/index.d.ts
```

## Defaults

- Endpoint: `http://127.0.0.1:34343/mcp`
- Token file: `~/.config/neurostrata/daemon.token`
- Namespace: `NeuroStrata`
- strictMode: `true`
- Request timeout: 5000 ms
