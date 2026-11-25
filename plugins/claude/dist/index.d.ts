/**
 * @neurostrata/plugin-core — Shared ESM implementation for all NeuroStrata harness adapters.
 *
 * Zero `require()` calls and zero external npm runtime dependencies. Only Node built-ins
 * (node:fs, node:os, node:path) are imported. Every harness adapter (opencode, claude,
 * codex, github-cli) is a thin wrapper that re-exports this module and builds one plugin
 * via `createHarnessPlugin`.
 *
 * Implements:
 *  - Bug 2: findProjectRoot / discoverProjectNamespace / resolveConfig (never hardcodes "NeuroStrata")
 *  - Bug 3: canonical TOOL_CLASSIFICATION + is*Tool predicates + OpenCode V2 Code Mode inspector
 *  - Bug 4: handleHookEvent / handleClaudeHookEvent (PreToolUse guard + subagent + recall)
 *  - Bug 5: prefetchMemoriesForFile + formatFileTouchRecallBlock + recordLocalSessionQuery + execute.after recall delivery
 *  - Enhancement 1: resolveActiveTaskFromDaemon (daemon-backed active task/session state)
 *  - Enhancement 2 & 3: buildSubagentContextHeader / injectSubagentPrompt / extractRepoFilePaths / enrichAndInjectSubagentPrompt
 *  - Enhancement 5: shared core architecture across all harness plugins
 */
export declare const DEFAULT_ENDPOINT = "http://127.0.0.1:34343/mcp";
export declare const DEFAULT_TOKEN_FILE = "daemon.token";
export declare const REQUIRED_TOOLS: string[];
export interface PluginConfig {
    namespace: string;
    endpoint: string;
    bearerToken: string | null;
    tokenFile: string | null;
    strictMode: boolean;
    requestTimeoutMs: number;
    intentDetection: {
        enabled: boolean;
        injectReminder: boolean;
    };
    /** Resolved project root directory (set in initialize from ctx.location / cwd). */
    projectRoot?: string;
}
/** Alias kept for readability in handler signatures (event, config: ResolvedConfig). */
export type ResolvedConfig = PluginConfig;
/** Allowlist namespace/task tokens before code-span interpolation (#316). */
export declare function sanitizeSubagentHeaderToken(value: string): string;
/**
 * Resolve `namespace` in strict priority order, the same order as
 * `neurostrata-mcp namespace resolve`:
 *  1. the project root marker (`.NeuroStrata/config.json`, findRootMarker(cwd))
 *  2. pluginConfig.namespace (the harness config; non-empty trimmed string)
 *  3. process.env.NEUROSTRATA_NAMESPACE (trimmed, one quote pair stripped and
 *     validated by `usableNamespace`, the CLI's `usable_env_namespace` rule)
 *  4. discoverProjectNamespace(cwd)
 * Never falls back to a hardcoded "NeuroStrata".
 */
export declare function resolveConfig(pluginConfig?: unknown, cwd?: string): PluginConfig;
/** Which walk found a project root marker. */
export type RootMarkerVia = 'own' | 'main' | 'nogit';
export interface FoundRootMarker {
    /** The project root, in the start path's own checkout. */
    root: string;
    namespace: string;
    via: RootMarkerVia;
}
/**
 * A namespace value as written, or undefined when it is not one: empty, an
 * unexpanded variable, or anything but `[A-Za-z0-9_.:-]` starting with a
 * letter or digit. The Rust installer's `usable_namespace` rule.
 */
export declare function usableNamespace(raw: unknown): string | undefined;
/**
 * The marker that decides `startDir`'s project root, or null. `startDir`
 * must be absolute on this host; for a file the walk starts at its folder.
 *  1. Own checkout: from the start folder up to its checkout top, inclusive;
 *     the deepest marker wins.
 *  2. Linked worktree: the start folder mapped into the main checkout,
 *     walked from its nearest existing folder up to the main checkout's top,
 *     never entering a folder with its own `.git`. Steps 1 and 2 both run;
 *     the deeper marker wins, at equal depth the own checkout's.
 *  3. Outside git: the first marker above the start folder.
 * When the winning marker cannot be read the answer is null, with one
 * warning line naming the file: the caller falls back to the other namespace
 * sources, never to a marker further up.
 */
export declare function findRootMarker(startDir: string): FoundRootMarker | null;
/**
 * Walk up from resolve(startDir) looking for `.git` (dir or file); return the
 * directory containing it. With no `.git` above (pass 2), the nearest folder
 * holding a valid project marker (`.NeuroStrata/config.json`), else the
 * nearest folder holding `.NeuroStrata` other than the home folder, else
 * resolve(startDir).
 */
export declare function findProjectRoot(startDir?: string, home?: string): string;
/**
 * The namespace `harness`'s plugin config names for the project at
 * `projectDir`, read from its checkout's top, then (for a linked worktree,
 * whose untracked configs usually live only in the main checkout) the main
 * checkout's; outside git from `projectDir` itself. Empty when none names a
 * usable namespace.
 *
 * `runnerDir` is the folder of the hook runner the installer wrote,
 * `<root>/.NeuroStrata/plugins/<harness>/`. The session's checkout is read
 * FIRST, then the installer's root: a shared runner must not answer for
 * another project (#378 F5). A config that exists but cannot be read or
 * parsed is reported and fails closed, never falling through to env/folder
 * discovery (#378 F9).
 */
export declare function readHarnessConfig(harness: string, projectDir: string, runnerDir?: string): {
    namespace?: string;
};
/**
 * The project folder a CLI hook runner resolves its namespace from. Claude
 * Code starts a new hook process per event, and the event's `cwd` follows
 * every `cd`, so for Claude this is the session's project folder:
 * `CLAUDE_PROJECT_DIR`, which Claude Code exports to hook processes as the
 * project root where the session started. Without it, the folder recorded
 * for this session at SessionStart (`claude_session_dirs` in
 * `.NeuroStrata/active_session.json`; never the home folder's), else the
 * event's `cwd`. Other harnesses use the event's `cwd`, else the process's.
 *
 * SessionStart also fires on resume, clear and compact, when the event's
 * `cwd` may have moved: a folder already recorded for the session is kept.
 * The file is replaced whole (temp + rename), and never when it exists but
 * cannot be parsed, so the other fields it holds are not lost.
 */
export declare function resolveHookProjectDir(harness: string, event?: Record<string, unknown>, env?: Record<string, string | undefined>, home?: string): string;
/**
 * Whether `toolName` is one of [SESSION_SCOPED_TOOLS], bare or as a harness
 * spells an MCP tool (`mcp__<server>__neurostrata_task_claim`,
 * `<server>_neurostrata_task_claim`, `<server>.neurostrata_task_claim`).
 */
export declare function isSessionScopedTool(toolName: string): boolean;
/**
 * Claim scope from a linked worktree (design 6.5): `neurostrata_task_claim`
 * and `neurostrata_get_snapshot` without `worktree`, `project_root` or `cwd`
 * are scoped by the daemon to the namespace's registry root, the main
 * checkout when a marker applies. This passes the session's project folder
 * as `worktree` instead. Changes `args` in place; true when it did.
 */
export declare function addSessionWorktree(toolName: string, args: Record<string, unknown>, projectDir: string | undefined): boolean;
/**
 * 1. Walk up to findProjectRoot(startDir) checking <dir>/.NeuroStrata/session_queries.json;
 *    if it parses as JSON with a non-empty string .namespace, return it trimmed.
 * 2. Otherwise return basename(findProjectRoot(startDir)) || "default".
 */
export declare function discoverProjectNamespace(startDir?: string): string;
/**
 * Resolve the project root from an OpenCode V2 host context, falling back
 * through ctx fields to process.cwd(). Priority:
 *   1. ctx.location.project.canonical
 *   2. ctx.location.project.root
 *   3. ctx.location.directory
 *   4. ctx.projectDir
 *   5. ctx.directory
 *   6. ctx.cwd
 *   7. process.cwd()
 */
export declare function resolveProjectRoot(ctx?: any): string;
export declare function daemonTokenPath(tokenFile?: string | null): string;
export declare function loadDaemonToken(tokenFile?: string | null): string | null;
export declare function buildHeaders(token: string | null): Record<string, string>;
export interface JsonRpcRequest {
    jsonrpc: '2.0';
    id: number;
    method: string;
    params?: unknown;
}
export interface JsonRpcResponse {
    jsonrpc?: string;
    id?: number | string | null;
    result?: unknown;
    error?: {
        code?: number;
        message?: string;
    };
}
export interface McpToolResult {
    content?: Array<{
        type?: string;
        text?: string;
    }>;
    isError?: boolean;
}
export declare function buildToolCallRequest(id: number, name: string, args: unknown): JsonRpcRequest;
export declare function buildToolsListRequest(id: number): JsonRpcRequest;
export declare function extractToolText(result: unknown): string;
export declare function extractToolNames(result: unknown): string[];
export declare class NeuroStrataMCPClient {
    private readonly endpoint;
    private readonly token;
    private readonly timeoutMs;
    private requestId;
    /** Daemon memory advisory captured on the last errored tool call, drained by
     *  the plugin's execute.after handler so the agent sees it as a suffix line
     *  rather than an opaque throw. Cleared on every callTool. */
    private pendingDaemonAdvisory;
    constructor(options: {
        endpoint: string;
        token: string | null;
        timeoutMs: number;
    });
    /** Drain the advisory captured by the last errored callTool, if any. */
    flushPendingAdvisory(): string | null;
    callTool(name: string, args: unknown): Promise<string>;
    listToolNames(): Promise<string[]>;
    private post;
}
export interface GuardValidateResult {
    verdict: 'pass' | 'reject';
    bucket: string;
    reasons: string[];
    constraints: string[];
    notes: string[];
    rule_ids: string[];
}
export declare function parseGuardValidate(text: string): GuardValidateResult;
export type AdapterState = 'uninitialized' | 'ready' | 'degraded' | 'unavailable';
export interface AdapterStatus {
    state: AdapterState;
    endpoint: string;
    namespace: string;
    authenticated: boolean;
    missingTools: string[];
    lastError?: string;
    updatedAt: string;
}
export declare function adapterStatus(): AdapterStatus;
export interface TaskIntentSignal {
    marker: string;
    excerpt: string;
}
export declare const MARKERS: readonly [string, string][];
export declare const NEGATION_PREFIXES: string[];
export declare function detectTaskIntentsLocal(text: string): TaskIntentSignal[];
export interface ProvenanceSpan {
    turn_id?: string;
    role?: string;
    source_tool?: string;
    channel?: string;
    evidence_quote?: string;
    quote?: string;
    char_range?: [number, number];
    parent_task_id?: string;
    confidence?: number;
}
export interface Turn {
    role: string;
    content: string;
    turn_id?: string;
    source_tool?: string;
    channel?: string;
    parent_task_id?: string;
}
export interface TurnCandidate {
    kind: 'task_obligation' | 'memory_candidate';
    category?: 'fact' | 'decision' | 'rule_candidate';
    title: string;
    description?: string;
    confidence: number;
    auto_commit: boolean;
    provenance: ProvenanceSpan;
}
export interface ConceptualBridge {
    source_id?: string;
    via_id?: string;
    target_id?: string;
    target_domain?: string;
    target_type?: string;
    target_summary?: string;
    path?: string;
}
/**
 * Extract task-obligation candidates from a turn's content. Detects local
 * task-intent markers (TODO:, FIXME:, follow-up:, ...) via
 * `detectTaskIntentsLocal` and attaches a provenance span
 * (turn_id, role, source_tool, channel, evidence_quote, quote, char_range,
 * confidence) to each. Pure: no daemon interaction. Rule candidates are
 * never produced here (rule candidates never auto-promote).
 */
export declare function extractTurnCandidates(turn: Turn): TurnCandidate[];
/**
 * Post turn candidates fail-soft to the intake ledger via
 * `neurostrata_task_candidates` (`action: "record"`). Never throws: a missing
 * MCP client or a daemon error is swallowed so a turn is never blocked by
 * intake-ledger submission.
 */
export declare function postTurnCandidates(candidates: TurnCandidate[], namespace: string): Promise<void>;
/**
 * Extract de-duplicated conceptual bridges from `memories_for_file` hits.
 * Each hit may carry a `conceptual_bridges` array (source -> via -> target
 * 2-hop cross-domain paths). Bridges are de-duplicated by `path`.
 */
export declare function extractConceptualBridges(hits: unknown[]): ConceptualBridge[];
/**
 * Persist conceptual bridges into `.NeuroStrata/active_session.json` under
 * the `conceptual_bridges` key, merging with any existing fields so the
 * `sessionContextHandler` can inject them as `[NeuroStrata Conceptual Bridges]`.
 */
export declare function persistConceptualBridges(projectRoot: string, bridges: ConceptualBridge[]): void;
export declare const READ_FILE_TOOLS: Set<string>;
export declare const MUTATING_FILE_TOOLS: Set<string>;
export declare const SHELL_TOOLS: Set<string>;
export declare const MUTATING_TOOLS: Set<string>;
export declare const SUBAGENT_TOOLS: Set<string>;
export declare const CODE_EXEC_TOOLS: Set<string>;
export declare const TOOL_CLASSIFICATION: {
    READ_FILE_TOOLS: Set<string>;
    MUTATING_FILE_TOOLS: Set<string>;
    SHELL_TOOLS: Set<string>;
    MUTATING_TOOLS: Set<string>;
    SUBAGENT_TOOLS: Set<string>;
    CODE_EXEC_TOOLS: Set<string>;
};
export declare function isFileTouchTool(name: string): boolean;
export declare function isMutatingTool(name: string): boolean;
export declare function isReadOnlyInspectionTool(name: string): boolean;
export declare function isSubagentTool(name: string): boolean;
export declare function isCodeExecTool(name: string): boolean;
/**
 * True only for shell tools whose payload is provably read-only/verification.
 * JSON payloads are unwrapped via `command`/`cmd`; substitutions, redirection
 * and chains containing any mutating segment fail closed.
 */
export declare function isZeroActionExemptShellPayload(actionType: string, payloadOrCommand: string): boolean;
export interface CodeModeInspection {
    id?: string;
    sessionId?: string;
    note?: string;
    taskClaimed?: boolean;
    taskUpdated?: boolean;
    taskCompleted?: boolean;
    memoryMutated?: boolean;
    queriedFiles: string[];
}
/**
 * Inspect JS code strings passed to OpenCode V2 `execute` (Code Mode).
 * Detects neurostrata_task_claim / _update / _complete / _memories_for_file
 * calls in order and extracts id / session_id / note / path arguments from
 * each matched call's own argument slice.
 */
export declare function inspectCodeModeExecution(code: string): CodeModeInspection;
export declare const PREFETCH_LRU_CAP = 128;
export declare const PREFETCH_MAX_CHARS = 2048;
export declare const prefetchedPaths: Map<string, number>;
export declare const injectedMemoryIds: Set<string>;
export interface FileMemoryHit {
    id: string;
    valid_from: string;
    memory_type: string;
    content: string;
}
export declare function parseFileMemoryHits(text: string): FileMemoryHit[];
/**
 * Recall governing memories for a touched file. LRU path dedup (no re-query within the
 * window) and injectedMemoryIds dedup (each memory injected once per session).
 */
export declare function prefetchMemoriesForFile(namespace: string, filePath: string, opts?: {
    recordLedgerDir?: string;
    projectRoot?: string;
}): Promise<string[]>;
/**
 * Mark a prefetch as delivered: its memories were handed to the caller, so the
 * dedup state stays and a later touch of the same file does not re-inject
 * them.
 */
export declare function confirmPrefetchDelivery(namespace: string, filePath: string): void;
/**
 * Roll back the dedup state of a prefetch whose recall was never delivered
 * (the guard denied the edit). The path is un-marked and the memory ids the
 * prefetch consumed are returned, so the allowed retry still sees the
 * governing rules.
 */
export declare function restorePrefetchDelivery(namespace: string, filePath: string): void;
export declare function formatFileTouchRecallBlock(filePath: string, snippets: string[]): string;
export declare const SUBAGENT_CONTEXT_MARKER = "### NeuroStrata Task & Graph Context";
export declare function isReadOnlySubagentAgent(agentId?: string | null): boolean;
/** Resolve the subagent agent id from the fields harnesses use to name it. */
export declare function resolveSubagentAgentId(target: Record<string, unknown>): string | undefined;
/** Code-Mode harnesses (OpenCode V2) expose MCP via `execute` + tools.neurostrata.*. */
export declare function detectCodeMode(ctx?: any, cfg?: PluginConfig): boolean;
export interface SubagentHeaderOptions {
    agentId?: string | null;
    codeMode?: boolean;
}
/**
 * Render a value inside a markdown backtick code span without allowing a
 * breakout: backticks close the span and newlines end its line, so both are
 * collapsed to single spaces. The namespace is not privileged input — it can
 * arrive from a committed `.NeuroStrata/session_queries.json`, plugin config,
 * or an environment variable — so every header interpolation goes through
 * here (issue #316).
 */
export declare function markdownCodeSpanValue(value: unknown): string;
/**
 * Public header builder: sanitizes the two caller-supplied values before
 * rendering (issue #316).
 */
export declare function buildSubagentContextHeader(namespace: string, taskId: string | null, extraContext?: string, options?: SubagentHeaderOptions): string;
export declare function injectSubagentPrompt(target: Record<string, unknown>, namespace: string, taskId: string | null, extraContext?: string, options?: SubagentHeaderOptions): boolean;
export declare function extractRepoFilePaths(promptText: string): string[];
export declare const cachedTaskInjection: Map<string, string | null>;
export declare const cachedSubagentPromptByTask: Map<string, string | null>;
/**
 * Resolve the active task from the daemon (fail-soft). Caches activeTaskId,
 * activeSessionId and cachedTaskInjection.
 */
export declare function resolveActiveTaskFromDaemon(namespace: string, preferredTaskId?: string | null): Promise<{
    taskId: string | null;
    sessionId: string | null;
    promptInjection: string | null;
}>;
/**
 * Prefetch governing memories for repo file paths mentioned in a subagent prompt and
 * inject the daemon's subagent_prompt_injection plus any recalled snippets into the
 * target prompt. Idempotent.
 */
export declare function enrichAndInjectSubagentPrompt(target: Record<string, unknown>, namespace: string, taskId?: string | null): Promise<boolean>;
/**
 * Set `args.session_id` to the harness's own session id for task_claim and
 * task_update, under any MCP prefix (`mcp__neurostrata__neurostrata_task_claim`
 * in Claude Code, `neurostrata_neurostrata_task_claim` in OpenCode). The agent
 * cannot see that id and often passes a label, so the claim would not match
 * the session that later reads active_session.json. Returns true when changed.
 */
export declare function stampHarnessSessionId(toolName: string, args: Record<string, unknown> | undefined, sessionId: string | undefined): boolean;
export declare let activeSessionId: string | null;
export declare let activeTaskId: string | null;
export declare let mutationsSinceNote: number;
export declare function resetState(): void;
export declare function resetClientForTesting(): void;
export declare function getTrackingState(): {
    activeTaskId: string | null;
    activeSessionId: string | null;
    mutationsSinceNote: number;
};
/** Structured marker the daemon writes on its error/diagnostic channel when
 *  it reports buffer-pool / RSS pressure. Memory payloads and tool-result
 *  rows never carry it, so matching it can never misfire on user content. */
export declare const DAEMON_MEMORY_ADVICE_MARKER = "NEUROSTRATA_ADVICE[daemon_memory]";
export declare function resetAdvisoryStateForTesting(): void;
/** Drain the live client's pending advisory (if any). Returns-and-clears. */
export declare function flushPendingDaemonAdvisory(): string | null;
/** Pull the daemon memory advisory out of the error/diagnostic channel.
 * Only text carrying the marker is inspected — memory payloads and result
 * rows are never matched, so a memory that merely discusses
 * `NEUROSTRATA_BUFFER_POOL_MB` cannot synthesize an advisory. Returns the
 * guidance string once per 10-minute window, or `null` when already shown
 * or absent. */
export declare function extractDaemonMemoryAdvisory(errorChannelText: string | null | undefined, nowMs?: number): string | null;
export interface SessionEpochState {
    epoch: number;
    baselineInjectedEpoch: number;
    seenFiles: Set<string>;
    /** Session-start keys (`<id>#<revision>`) injected in this epoch. */
    injectedKeys: Set<string>;
    /** The epoch whose session file was last refreshed from a snapshot. */
    refreshedEpoch: number;
    /** The snapshot call in flight, shared by concurrent context hooks. */
    refreshInFlight?: Promise<SnapshotOutcome>;
    /** When the last snapshot call failed (ms), to pause before a retry. */
    refreshFailedAt?: number;
    /** When the snapshot of `refreshedEpoch` ran (ms), for the opening line. */
    refreshedAt?: number;
    /** The session this one replaced through a `clear` start, if any. */
    parentSessionId?: string;
    /** This epoch's snapshot did not run at session start (SnapshotPending). */
    snapshotPending?: SnapshotPending;
    /** The last snapshot call's outcome. */
    lastRefresh?: SnapshotOutcome;
}
/**
 * A session start whose get_snapshot the daemon did not answer: the session
 * got the saved session file, and the snapshot follows on a later call (the
 * first tool round; on OpenCode the next request if it comes first). Saved
 * in session_epochs.json as `snapshot_pending`, for its own epoch only.
 */
export interface SnapshotPending {
    epoch: number;
    /** When the session start found the daemon not answering (ms). */
    since_ms: number;
    /** The sections that session start asked for. */
    sections?: string[];
    /** The session start's `source` (startup, resume, clear, compact...). */
    source?: string;
    /** The snapshot ran on a later call; its text is not delivered yet (ms). */
    ran_at?: number;
    /** The last retry failed (ms), and how. */
    failed_at?: number;
    failure?: SnapshotOutcome['result'];
    failure_detail?: string;
    /** The user and the agent were told the retry failed (once per epoch). */
    told?: boolean;
    /** A hook calling get_snapshot for it now (resolvePendingSnapshot). */
    claim?: {
        by: string;
        at: number;
    };
}
/** What one get_snapshot call did. */
export interface SnapshotOutcome {
    result: 'refreshed' | 'refused' | 'timeout' | 'error';
    detail?: string;
    ms: number;
}
/** Current epoch for a session (starts at 0, incremented by compaction). */
export declare function currentSessionEpoch(sessionId: string | null | undefined): number;
/** Increment the epoch and clear seen state so the next context hook re-injects. */
export declare function incrementSessionEpoch(sessionId: string | null | undefined): void;
export declare function sessionSeenFiles(sessionId: string | null | undefined): Set<string>;
export declare function resetSessionEpoch(sessionId: string | null | undefined): void;
export declare function resetAllSessionEpochs(): void;
/** Load the session's persisted epoch state into memory (hook processes). */
export declare function loadPersistedSessionEpoch(root: string, sessionId: string): void;
/**
 * Save the session's epoch state; a missed write only repeats text on
 * resume. The file holds every session of the project and the hooks of
 * parallel tool calls write it too, so the read-modify-write runs under the
 * session-state lock (without it after STATE_LOCK_WAIT_MS: a hook never
 * hangs).
 */
export declare function persistSessionEpoch(root: string, sessionId: string): void;
/** How long a hook waits for another hook's state lock. */
export declare const STATE_LOCK_WAIT_MS = 2000;
/** A lock older than this was left by a hook that died; the next takes it. */
export declare const STATE_LOCK_STALE_MS = 10000;
/**
 * Run `fn` while holding the lock file `lockPath` (created exclusively).
 * Claude Code runs the hooks of parallel tool calls as parallel processes,
 * and each reads, changes and writes the same state files. The lock covers
 * file I/O only, never a daemon call. A hook waits at most `waitMs`; without
 * the lock `fn` does not run and `held` is false, and the caller decides
 * (send nothing owed, or write without it). Re-entrant in one process.
 */
export declare function withLockFile<T>(lockPath: string, fn: () => T, waitMs?: number, staleMs?: number): {
    held: boolean;
    value?: T;
};
/**
 * The session-start limit, in UTF-8 bytes. Claude Code caps hook context at
 * 10,000 JavaScript characters (past it: a file path and the first 2,000);
 * Codex 0.170 at 2,500 tokens counted as UTF-8 bytes / 4, about 10,000 bytes
 * (past it the text goes to a temp file and the model sees a preview). A
 * string's UTF-8 bytes are never fewer than its UTF-16 units, so one byte
 * check under 9,500 keeps both harnesses whole.
 */
export declare const SESSION_START_MAX_BYTES = 9500;
/** The UTF-8 size of `text`, what the session-start limit counts. */
export declare function utf8Bytes(text: string): number;
/**
 * The plugin's own framing around the daemon's lines: section headings, the
 * task line, the handoff heading, the resume notice, the "  - " prefix and
 * line break per line and the whole-rule hint on cut golden rules.
 */
export declare const SESSION_START_FRAMING_BYTES = 1500;
/** The `max_bytes` the plugin sends to get_snapshot (UTF-8 bytes too). */
export declare const SESSION_START_CONTENT_BUDGET: number;
/** One piece of session-start text that is kept or left out as a whole. */
export interface SessionStartUnit {
    text: string;
    kind: 'handoff' | 'rule' | 'other';
    /** Dedup key `<id>#<revision>`; headings and notices have none. */
    key?: string;
}
/**
 * How a session start relates to what the context already holds. Only an
 * explicit resume reloads the earlier injections (Claude Code and Codex
 * `resume`, Copilot `resume`) and stays in the same epoch; startup, clear,
 * compact, Codex `fork`, Copilot `new` and a missing value start a new one.
 */
export declare function sessionStartIsResume(source: string | undefined): boolean;
/**
 * A memory's whole content from neurostrata_get_memory's reply, or
 * undefined for a miss or a retired memory (its reply opens with a banner
 * before `--- Memory ID:`).
 */
export declare function memoryContentFromReply(text: string): string | undefined;
/** Rules left out entirely that come whole on the next tool calls; the
 * rest come once as first sentences. */
export declare const LEFT_OUT_WHOLE_RULES = 5;
/**
 * What session start left out, delivered on the next tool calls of the same
 * epoch through the file-recall injection (the hook's additionalContext):
 * Golden Rules shown only by first sentence (over their share) and a handoff
 * note the fit left out first, then the LEFT_OUT_WHOLE_RULES most relevant
 * rules left out entirely, whole, then the rest once by first sentence.
 * Whole text is read by id with neurostrata_get_memory. Each item goes in
 * once per epoch (its key in the epoch state, as session start's own):
 * `whole:<id>` for a whole rule, `listed:<id>` for a first-sentence line, so
 * a session file rewritten with a changed id list (a claim, a task update)
 * sends only the rules not sent yet. Items that do not fit `budgetBytes`
 * wait for a later call; a new epoch starts over. Rules not listed at all
 * (`also_on_record_left_out`) have no id here and stay with session start's
 * closing line. Hook harnesses (`persist`) read without a lock (the daemon
 * calls), then pick and mark under the session-state lock against the saved
 * keys, so the hooks of parallel tool calls never send an item twice; a hook
 * that does not get the lock sends nothing. Never throws.
 */
export declare function leftOutSessionStartBlock(root: string, sessionId: string | undefined, budgetBytes: number, persist: boolean): Promise<string | undefined>;
export interface SessionStartOptions {
    sessionId?: string;
    /** Show the task line and block only to the session that claimed it. */
    ownTaskOnly?: boolean;
    /**
     * With ownTaskOnly: the session ids that count as the claiming session
     * (ownSessionIds); default the session id alone.
     */
    ownSessionIds?: string[];
    /** Keys this epoch already injected (resume, repeat context hooks). */
    skip?: Set<string>;
    /** Say how many items were left out as already seen (hook resume). */
    announceSkipped?: boolean;
    namespace?: string;
}
/**
 * Claude Code's /clear ends the session (SessionEnd, reason `clear`) and
 * starts one with a NEW session id (SessionStart, source `clear`); the hook
 * log shows every `clear` start with an id not seen before, while `compact`
 * and `resume` keep theirs. The cleared session is recorded so the next
 * `clear` start counts it as its predecessor.
 */
export declare function recordSessionCleared(root: string, sessionId: string): void;
/**
 * The session-file sections session start asks get_snapshot for, in fit
 * order (the daemon's `sections`). No task block at session start: the claim
 * reply carries it. A new session (startup, Copilot `new`, Codex `fork`) has
 * no task until it claims one; a continuing one (resume, clear, compact) gets
 * its task line, and on resume the seen-dedup leaves only what changed.
 */
export declare function sessionStartSections(continuing: boolean): string[];
/**
 * The session ids whose claimed task counts as this session's own at a
 * session start. None for a new session (startup, Copilot `new`, Codex
 * `fork`, or no start value): it has no goal until the agent claims or
 * resumes a task, and the claim reply carries the context. On resume and
 * compact, the session itself; on `clear`, also the session it replaced (the
 * latest one cleared within SESSION_CLEAR_LINK_MS), followed back through
 * earlier clears. Call after loadPersistedSessionEpoch; persist afterwards.
 */
export declare function ownSessionIds(root: string, sessionId: string | undefined, source: string | undefined): string[];
/**
 * The task block as injected at session start: without the paragraphs
 * AGENTS.md already carries (the Graph-First Lookup Rule and the Formal
 * Verification Reflex). The block in the session file and in claim replies
 * is unchanged.
 */
export declare function taskBlockForInjection(block: string): string;
/**
 * The session-start units built from active_session.json, in the daemon's
 * priority order: handoff note, task line and block, golden rules, namespace
 * rules, also on record; then conceptual bridges. Each rule id appears once.
 */
export declare function buildSessionStartUnits(active: Record<string, unknown>, options?: SessionStartOptions): SessionStartUnit[];
/**
 * Fit units under `max` UTF-8 bytes without cutting one: units are kept in
 * order, a unit that does not fit is left out (a later, shorter one may still
 * fit), and closing lines say what was left out and how to reach it. The
 * guard for a session file the daemon did not fit (an older daemon).
 */
export declare function fitSessionStartUnits(units: SessionStartUnit[], max?: number): {
    text: string;
    shownKeys: string[];
};
/**
 * The session-start text: what the daemon fitted to our budget (the file's
 * `max_bytes`) goes in as it is, with a closing line when the daemon left
 * rules out; a file without `max_bytes` (an older daemon), or fitted to a
 * larger budget by another caller, goes through the whole-line guard.
 */
/**
 * Closing lines for what the daemon left out to fit the budget (spec 5.3:
 * truncation is always reported). `namespace_rules_left_out` counts the
 * rules not shown whole; `also_on_record` lists some of those by first
 * sentence and `also_on_record_left_out` counts the rest, so each rule is
 * counted once. The task lines appear only for the session's own task.
 */
export declare function daemonLeftOutLines(active: Record<string, unknown>, ownTask: boolean): string[];
export declare function sessionStartText(active: Record<string, unknown>, units: SessionStartUnit[], max?: number, ownTask?: boolean, 
/** Bytes the caller adds to the text (the snapshot status line). */
reserve?: number, 
/** Keys this epoch already showed: closing lines shown are not repeated. */
skip?: Set<string>): {
    text: string;
    shownKeys: string[];
};
/**
 * SessionStart for every hook-based harness (Claude Code, Codex, Copilot):
 * load the session's epoch state, start a new epoch unless this is a resume,
 * inject through sessionContextHandler, save the state. One get_snapshot
 * call, no wait: when the daemon does not answer, the saved session file goes
 * in, the user gets one line, and the first tool round loads the snapshot
 * (resolvePendingSnapshot, pendingSessionStartBlock). Never throws; a
 * failure injects nothing.
 */
export declare function handleSessionStartEvent(event: Record<string, unknown>, dialect?: 'claude' | 'copilot'): Promise<Record<string, unknown>>;
/**
 * How long after a session start a refused get_snapshot still counts as the
 * daemon starting up (the harness starts it after the session-start hook;
 * measured: it listens about 4.5 s later). A tool round inside this window
 * retries until the mark; a refusal after it is a failure.
 */
export declare const SNAPSHOT_WAIT_MS = 10000;
/** Pause between retries of a refused get_snapshot inside SNAPSHOT_WAIT_MS. */
export declare const SNAPSHOT_RETRY_GAP_MS = 250;
/** The user's one line when session start got no snapshot. */
export declare function snapshotDeferredUserLine(outcome: SnapshotOutcome): string;
/** The user's line when the later retry failed too: what failed, what to do. */
export declare function snapshotFailedUserLine(failure: {
    result: SnapshotOutcome['result'];
    detail?: string;
}): string;
/** The agent's opening line while the snapshot is pending. */
export declare function snapshotPendingLine(hasFile: boolean): string;
/** The agent's opening line for a snapshot that ran on a tool round. */
export declare function snapshotRanNowLine(ranAt: number): string;
/**
 * The agent's line when the retry failed too. OpenCode has no channel to the
 * user, so there it carries `tellUser` for the agent to pass on.
 */
export declare function snapshotFailedAgentLine(failure: {
    result: SnapshotOutcome['result'];
    detail?: string;
}, tellUser?: string): string;
/**
 * What the model reads with an OpenCode user line. The user sees only the
 * synthetic message's `description`; its `text` goes to the model as a user
 * message, so it says what the line is and that nothing is asked of it.
 */
export declare function openCodeUserLineNote(message: string): string;
/** What a tool round delivers for a pending snapshot. */
export interface PendingSessionStart {
    /** For the agent: the snapshot's session-start text, or the failure line. */
    text: string;
    /** The session-start keys `text` shows; marked when a reply carries it. */
    shownKeys: string[];
    /** All the snapshot brought is in `text`: the wait ends with this reply. */
    complete: boolean;
    /** For the user: the failure, once per epoch. */
    userLine?: string;
}
/**
 * The text a tool round owes for the epoch's pending snapshot, within
 * `budgetBytes`: after the snapshot ran, the session-start text through the
 * same builder session start uses (what the saved file shown at session start
 * did not have, by the epoch's keys), opened by snapshotRanNowLine; after a
 * failed retry, the failure line once. A snapshot whose session file cannot
 * be read afterwards counts as a failed retry (told, retried after the
 * pause). Nothing is marked: commitPendingSessionStart does that when a reply
 * carries the text. Undefined when nothing is owed or nothing fits (it waits
 * for a later call).
 */
export declare function pendingSessionStartBlock(root: string, sessionId: string | undefined, budgetBytes: number): PendingSessionStart | undefined;
/** A reply carried `delivery`: mark its keys; a complete one ends the wait. */
export declare function commitPendingSessionStart(sessionId: string | undefined, delivery: PendingSessionStart): void;
/**
 * Hook harnesses: what this tool round owes for the pending snapshot
 * (pendingSessionStartBlock), taken under the session-state lock against
 * the saved state and marked and saved before the reply goes out, so the
 * hooks of parallel tool calls never send it twice. `failureOnly` (a denial)
 * takes only the failure line. Undefined when nothing is owed, nothing fits,
 * or another hook holds the lock.
 */
export declare function takePendingSessionStart(root: string, sessionId: string, budgetBytes: number, failureOnly?: boolean): PendingSessionStart | undefined;
/** Tests capture progress lines instead of writing them to stdout. */
export declare function setHookProgressWriterForTesting(writer?: (line: string) => Promise<void>): void;
/**
 * postToolUse. Copilot's preToolUse reply cannot carry text on an allowed
 * call, but its postToolUse `additionalContext` is appended to the tool
 * result the model reads on the same turn (GitHub Docs, Copilot CLI hooks
 * configuration reference). So for Copilot this is where a tool round loads a
 * pending snapshot and delivers its session-start text (or tells the user,
 * through a progress line, that it failed), then what session start left
 * out: at most SESSION_START_MAX_BYTES (Copilot caps all hooks'
 * additionalContext together at 10 KB). Claude Code and Codex deliver on
 * PreToolUse, so for them this answers `{}`. Never throws.
 */
export declare function handlePostToolUseEvent(event: Record<string, unknown>, dialect?: 'claude' | 'copilot'): Promise<Record<string, unknown>>;
/**
 * The opening line of session-start text that came with a snapshot refresh:
 * the plugin's get_snapshot is the session's one call, so the agent is told
 * it ran (or that loading failed and it must call it itself).
 */
export declare function snapshotStatusLine(refreshedAt: number | undefined): string;
/**
 * `session.hook("context")` handler, and the one session-start injector for
 * every harness. Once per epoch (hook harnesses and OpenCode V2) it asks
 * for a snapshot (which rewrites
 * `.NeuroStrata/active_session.json` fitted to our budget), then injects what
 * this epoch has not shown yet: everything after a new epoch, only new or
 * changed items on a repeat call or a resume, nothing (event untouched) when
 * nothing changed. OpenCode V2 builds each request's system prompt afresh and
 * reads back only `system`, `messages`, `tools` and `options`; so on V2 (an
 * event with a `system` array) the whole text is pushed into `system` on
 * every request.
 */
export declare function sessionContextHandler(event: any, config: ResolvedConfig, options?: {
    ownTaskOnly?: boolean;
    announceSkipped?: boolean;
    /** Ask for a snapshot once per epoch (default: OpenCode V2 only). */
    refresh?: boolean;
    /** See SessionStartOptions.ownSessionIds. */
    ownSessionIds?: string[];
    /** The sections to ask get_snapshot for (sessionStartSections). */
    sections?: string[];
    /** UTF-8 bytes the caller adds to the injected text afterwards. */
    reserveBytes?: number;
    /** The session start's `source`, kept with a pending snapshot. */
    source?: string;
}): Promise<void>;
/**
 * OpenCode's model messages (`role` with string or `{type: "text"}` parts
 * content) as the transcript lines buildHandoffSummary reads.
 */
export declare function openCodeTranscriptLines(messages: unknown): string[];
/**
 * OpenCode session end: the server's `location.shutdown` event or the
 * plugin's unload (setup's cleanup). OpenCode has no per-session end event,
 * so every session that ran a request since its last handoff gets one, from
 * the messages its last request carried. Once per session until it runs
 * again.
 */
export declare function writeOpenCodeSessionEndHandoffs(): Promise<void>;
/** OpenCode bus event: refresh the session file as soon as a session is created. */
export declare function sessionCreatedHandler(event: any): Promise<void>;
/** The handoff summary from transcript lines (oldest first), or '' when empty. */
export declare function buildHandoffSummary(lines: string[], event: Record<string, unknown>): string;
export interface HandoffWriteOutcome {
    action: 'task_note' | 'log' | 'empty' | 'skipped' | 'daemon_error';
    task_id?: string;
    detail?: string;
    redactions?: HandoffRedactions;
}
/** What the daemon scrubbed from a handoff note: how many, of which types. */
export interface HandoffRedactions {
    count: number;
    types: string[];
}
/**
 * The secrets the daemon scrubbed from a note before storing it, read from
 * the reply's `redactions` list (one entry per scrubbed secret, naming its
 * `category`); undefined when it names none or the reply is not JSON. Only
 * the count and the types are kept, never any text.
 */
export declare function handoffRedactions(reply: string): HandoffRedactions | undefined;
/**
 * SessionEnd / PreCompact: store the handoff summary in NeuroStrata. Never
 * throws; never writes a file of its own.
 */
export declare function writeSessionHandoff(event: Record<string, unknown>, 
/** The transcript as lines, when the harness passes messages (OpenCode). */
transcriptLines?: string[]): Promise<HandoffWriteOutcome>;
/**
 * The preToolUse reply in the shape Copilot CLI reads: top-level
 * `permissionDecision`, `permissionDecisionReason` (required on deny, shown
 * to the agent) and `modifiedArgs` (an object used instead of the original
 * arguments); `hookSpecificOutput` is not read, and empty output keeps
 * Copilot's default behavior (GitHub Docs, Copilot CLI hooks configuration
 * reference, preToolUse). Only a deny is passed on: an explicit allow would
 * skip Copilot's own permission flow, which the hook should leave as it is.
 * Copilot has no context field on preToolUse, so a recall block goes with a
 * deny's reason, where the agent reads it.
 */
export declare function copilotPreToolUseReply(reply: Record<string, unknown>, originalArgs: unknown): Record<string, unknown>;
/**
 * `session.hook("compaction")` handler, OpenCode's PreCompact: writes the
 * handoff note from `event.messages` (the transcript being summarized), then
 * starts a new epoch and clears seen files, so the very next context hook
 * re-injects the session-start text and the next tool results bring what it
 * leaves out again.
 */
export declare function sessionCompactionHandler(event: any, config: ResolvedConfig): Promise<void>;
export declare const pendingRecallBlocks: string[];
export declare const pendingRecallByCall: Map<string, string>;
/**
 * Tool-name extraction for execute.after events. OpenCode V2 passes
 * `event.name`; V1 / Claude / Hermes pass `event.tool` / `event.tool_name`.
 * `event.toolName` is checked first.
 */
export declare function extractToolName(event: any): string;
export declare function initialize(ctx: any): Promise<(() => Promise<void>) | undefined>;
export declare function handleHookEvent(event: Record<string, unknown>): Promise<Record<string, unknown>>;
export declare const handleClaudeHookEvent: typeof handleHookEvent;
export declare function onTurnEnd(turn: {
    role: 'user' | 'assistant';
    content: string;
}): Promise<void>;
export declare function fetchContextPlan(opts: {
    namespace: string;
    query?: string;
    file_path?: string;
    max_items?: number;
    max_bytes?: number;
    format?: 'json' | 'text';
}): Promise<string>;
export declare function createHarnessPlugin(pluginId?: string, harnessName?: string): {
    id: string;
    name: string;
    setup: typeof initialize;
    initialize: typeof initialize;
    handleHookEvent: typeof handleHookEvent;
    handleClaudeHookEvent: typeof handleHookEvent;
    onTurnEnd: typeof onTurnEnd;
    fetchContextPlan: typeof fetchContextPlan;
    adapterStatus: typeof adapterStatus;
};
export declare const plugin: ReturnType<typeof createHarnessPlugin>;
export default plugin;
