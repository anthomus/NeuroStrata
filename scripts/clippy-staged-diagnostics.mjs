#!/usr/bin/env node
// Clippy diagnostic filter module.
// Exports pure findStagedDiagnostics(messages, root, stagedPaths) — no I/O.
// Invokes main() only when executed as CLI entry point.
// Usage from test import: import { findStagedDiagnostics } from "./clippy-staged-diagnostics.mjs" (named export)

import { fileURLToPath } from 'node:url'
import { readdirSync, readFileSync, realpathSync } from 'node:fs'
import path from 'node:path'

/**
 * Finds staged Clippy diagnostics from a newline-delimited JSON stream.
 *
 * @param {string[]} messages - newline-delimited JSON parser messages
 * @param {string} root - repo root path (used for path normalization)
 * @param {string[]} stagedPaths - repo-relative staged file paths
 * @returns {string[]} rendered diagnostics matching a staged primary span
 */
function findStagedDiagnostics(messages, root, stagedPaths) {
  const rootNorm = root.replace(/\\/g, '/').replace(/\/$/, '')
  const stagedSet = new Set(stagedPaths.map((p) => normalizePath(p, rootNorm)))

  const matched = []

  for (const msg of messages) {
    if (!msg || typeof msg !== 'object') continue
    if (msg.reason !== 'compiler-message') continue
    if (!msg.message || typeof msg.message !== 'object') continue

    const spans = Array.isArray(msg.message.spans) ? msg.message.spans : []
    // Find primary span
    const primarySpan = spans.find((s) => s && s.is_primary === true)
    if (!primarySpan) continue

    const fileName = primarySpan.file_name
    if (!fileName) continue

    // Normalize the file path from the diagnostic using the supplied root
    const normFileName = normalizePath(fileName, rootNorm)
    if (!stagedSet.has(normFileName)) continue

    // Matched: staged warning or error
    const level = msg.message.level || 'warning'
    if (level !== 'warning' && level !== 'error') continue

    // Use rendered if present; otherwise build a useful fallback diagnostic
    // so a staged warning/error with missing rendered still counts as a match.
    let rendered = msg.message.rendered
    if (!rendered) {
      const lintText =
        typeof msg.message.message === 'string'
          ? msg.message.message
          : 'lint diagnostic'
      const line = primarySpan.line_start || 1
      const col = primarySpan.column_start || 1
      rendered = `${level}: ${lintText}\n --> ${normFileName}:${line}:${col}\n`
    }
    matched.push(rendered)
  }

  return matched
}

function normalizePath(p, rootNorm) {
  if (!p) return ''
  // Replace backslashes with forward slashes (Windows safety)
  let s = p.replace(/\\/g, '/')
  // Lexically collapse `.` and `..` segments (never touches disk)
  s = path.posix.normalize(s)
  // Strip leading ./
  while (s.startsWith('./')) s = s.slice(2)
  // Strip absolute root prefix
  if (s.startsWith(rootNorm + '/')) {
    s = s.slice(rootNorm.length + 1)
  } else if (s === rootNorm) {
    s = ''
  }
  return s
}

function existingRealPath(p) {
  try {
    return realpathSync(p)
  } catch {
    return p
  }
}

// Splits rustc dep-info (Make syntax) into rules. rustc escapes only spaces
// ("\ "), so any other backslash is a Windows path separator. rustc ends each
// target with ": " or a colon at the end of the line, so a colon inside a path
// (C:\..., /tmp/a:b/...) is not the rule separator. A line without that form
// splits at its first colon that is not a drive letter's.
function parseDepInfoRules(text) {
  const rules = []
  for (const logicalLine of text.replace(/\\\r?\n/g, ' ').split('\n')) {
    if (logicalLine.startsWith('#')) continue
    const targets = []
    const dependencies = []
    let current = targets
    let token = ''
    const chars = [...logicalLine.replace(/\r$/, '')]
    const endsTarget = (i) => i + 1 === chars.length || /\s/.test(chars[i + 1])
    const spaced = chars.some((char, i) => char === ':' && endsTarget(i))
    for (let i = 0; i < chars.length; i++) {
      const char = chars[i]
      if (char === '\\' && chars[i + 1] === ' ') {
        token += ' '
        i++
      } else if (
        char === ':' &&
        current === targets &&
        (spaced
          ? endsTarget(i)
          : !(token.length === 1 && /[A-Za-z]/.test(token)))
      ) {
        if (token) targets.push(token)
        token = ''
        current = dependencies
      } else if (/\s/.test(char)) {
        if (token) current.push(token)
        token = ''
      } else {
        token += char
      }
    }
    if (token) current.push(token)
    if (current === dependencies) rules.push({ targets, dependencies })
  }
  return rules
}

/** Returns staged .rs paths absent from every Cargo dep-info dependency set. */
function findUnbuiltStagedRustFiles(depInfoTexts, root, stagedPaths) {
  const lexicalRoot = path.resolve(root).replace(/\\/g, '/').replace(/\/$/, '')
  const canonicalRoot = existingRealPath(root)
    .replace(/\\/g, '/')
    .replace(/\/$/, '')

  // Map a path (absolute or repo-relative) to a repo-relative spelling under
  // the canonical root. Nonexistent staged files fall back to their lexical
  // path, so rewrite the lexical root prefix before comparing. This matters on
  // macOS, where os.tmpdir() is under /var but realpath() returns /private/var.
  function repoRelative(p) {
    const absolute = path.isAbsolute(p) ? p : path.join(canonicalRoot, p)
    let resolved = existingRealPath(absolute).replace(/\\/g, '/')
    if (resolved.startsWith(lexicalRoot + '/')) {
      resolved = canonicalRoot + resolved.slice(lexicalRoot.length)
    }
    return normalizePath(resolved, canonicalRoot)
  }

  const built = new Set()
  for (const text of depInfoTexts) {
    for (const { dependencies } of parseDepInfoRules(text)) {
      for (const dependency of dependencies) {
        const absolute = path.isAbsolute(dependency)
          ? dependency
          : path.join(canonicalRoot, dependency)
        const normalized = repoRelative(absolute)
        if (normalized.endsWith('.rs')) built.add(normalized)
      }
    }
  }
  return stagedPaths
    .filter((p) => p.endsWith('.rs'))
    .map((p) => repoRelative(p))
    .filter((p) => !built.has(p))
}

function artifactKey(p) {
  const key = path.resolve(p).replace(/\\/g, '/')
  return process.platform === 'win32' ? key.toLowerCase() : key
}

// Cargo reports a build script by its hard link, build-script-build, while
// rustc's .d rule names the hashed file beside it, build_script_build-<hash>.
// The unhashed spelling can only equal an artifact name that has no hash.
function unhashedSibling(key) {
  const name = path.posix
    .basename(key)
    .replace(/-[0-9a-f]{16}(?=\.[^.]*$|$)/, '')
    .replace(/_/g, '-')
  return path.posix.join(path.posix.dirname(key), name)
}

/**
 * Returns the dep-info texts written for artifacts this Cargo run reported.
 * The target directory may be shared with other builds and keep .d files from
 * earlier checkouts, which name repo-relative sources too; only a .d file whose
 * rule names one of this run's artifacts describes the staged content.
 */
function readRunDepInfo(targetDir, messages) {
  const targetKey = artifactKey(targetDir)
  const artifacts = new Set()
  for (const msg of messages) {
    if (!msg || msg.reason !== 'compiler-artifact') continue
    for (const file of Array.isArray(msg.filenames) ? msg.filenames : []) {
      if (typeof file !== 'string') continue
      const key = artifactKey(file)
      if (key.startsWith(targetKey + '/')) artifacts.add(key)
    }
  }
  const dirs = new Set([...artifacts].map((key) => path.posix.dirname(key)))
  const texts = []
  for (const dir of dirs) {
    for (const entry of readdirSync(dir, { withFileTypes: true })) {
      if (!entry.isFile() || !entry.name.endsWith('.d')) continue
      const text = readFileSync(path.join(dir, entry.name), 'utf8')
      const ours = parseDepInfoRules(text).some(({ targets }) =>
        targets.some((target) => {
          const key = artifactKey(target)
          return artifacts.has(key) || artifacts.has(unhashedSibling(key))
        })
      )
      if (ours) texts.push(text)
    }
  }
  return texts
}

/**
 * Returns the targets of packages inside the staged snapshot that Cargo
 * reported as fresh. The snapshot is a new checkout, so its crates are always
 * checked again; a fresh one means Cargo replayed another checkout's build
 * from a shared target directory, whose diagnostics and dep-info do not
 * describe the staged files. Path dependencies outside the snapshot keep
 * their files between runs and may be fresh.
 */
function findReusedWorkspaceArtifacts(messages, root) {
  const rootKey = artifactKey(existingRealPath(root))
  const reused = new Set()
  for (const msg of messages) {
    if (!msg || msg.reason !== 'compiler-artifact' || msg.fresh !== true) {
      continue
    }
    if (typeof msg.manifest_path !== 'string') continue
    const manifest = artifactKey(existingRealPath(msg.manifest_path))
    if (manifest.startsWith(rootKey + '/')) {
      reused.add(msg.target && msg.target.name ? msg.target.name : manifest)
    }
  }
  return [...reused]
}

/**
 * CLI entry point. Reads NDJSON from stdin, exits:
 *   1 — a staged warning/error primary span was found
 *   0 — no staged warning/error matches
 *   2 — malformed JSON or missing --root/--staged args
 */
function main() {
  const args = process.argv.slice(2)
  let root = ''
  let depInfoRoot = ''
  const staged = []

  for (let i = 0; i < args.length; i++) {
    if (args[i] === '--root' && i + 1 < args.length) {
      root = args[++i]
    } else if (args[i] === '--dep-info-root' && i + 1 < args.length) {
      depInfoRoot = args[++i]
    } else if (args[i] === '--staged') {
      // The runner passes every staged path after a single flag. Consume until
      // the next option instead of taking only the first path.
      while (i + 1 < args.length && !args[i + 1].startsWith('--')) {
        staged.push(args[++i])
      }
    }
  }

  if (staged.length === 0 || root === '') {
    console.error(
      'usage: clippy-staged-diagnostics.mjs --root <root> --staged <path> [...]'
    )
    process.exit(2)
  }

  let stdin = ''
  process.stdin.setEncoding('utf8')
  process.stdin.on('data', (chunk) => {
    stdin += chunk
  })
  process.stdin.on('end', () => {
    let messages
    try {
      messages = stdin
        .split('\n')
        .filter((line) => line.trim())
        .map((line) => JSON.parse(line))
    } catch (e) {
      console.error('error: malformed Clippy NDJSON input')
      process.exit(2)
    }

    if (!Array.isArray(messages)) {
      console.error(
        'error: Clippy output must be newline-delimited JSON objects'
      )
      process.exit(2)
    }

    const matched = findStagedDiagnostics(messages, root, staged)
    let unbuilt = []
    let reused = []
    if (depInfoRoot) {
      reused = findReusedWorkspaceArtifacts(messages, root)
      try {
        unbuilt = findUnbuiltStagedRustFiles(
          readRunDepInfo(depInfoRoot, messages),
          root,
          staged
        )
      } catch (error) {
        console.error(`error: cannot read Cargo dep-info: ${error.message}`)
        process.exit(2)
      }
    }

    for (const rendered of matched) {
      process.stdout.write(rendered)
      if (!rendered.endsWith('\n')) process.stdout.write('\n')
    }
    for (const file of unbuilt) {
      console.error(
        `error: staged Rust file ${file} is not in the Cargo build dep-info; add it to a target/module graph or unstage it`
      )
    }
    for (const name of reused) {
      console.error(
        `error: Cargo reused an earlier build of workspace target ${name} instead of checking the staged files; run the commit again`
      )
    }

    process.exit(
      matched.length > 0 || unbuilt.length > 0 || reused.length > 0 ? 1 : 0
    )
  })
}

// Export pure function for importing tests
export {
  findStagedDiagnostics,
  findReusedWorkspaceArtifacts,
  findUnbuiltStagedRustFiles,
  parseDepInfoRules,
  readRunDepInfo
}

// Run main() only when executed as CLI (not imported). Compare real paths so a
// symlinked checkout (macOS /tmp, iCloud, symlinked worktrees) still runs main().
function isMainModule() {
  if (!process.argv[1]) return false
  try {
    return (
      realpathSync(process.argv[1]) ===
      realpathSync(fileURLToPath(import.meta.url))
    )
  } catch {
    return false
  }
}
if (isMainModule()) {
  main()
}
