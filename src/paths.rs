//! # All path handling goes through `crate::paths`.
//!
//! **Do not normalize, join or compare paths by hand anywhere else.** Every
//! `replace('\\', "/")`, `strip_prefix("./")`, `canonicalize`, case fold or
//! "is this absolute?" test written at a call site is a second copy of a rule
//! that lives here, and the copies drift: the node-path normalizer existed
//! twice, the absolute-location test once more, and hooks, ledgers and file
//! lookups disagreed about the same file. If a rule you need is missing, add
//! it here, with a test for the Windows and the POSIX spelling.
//!
//! The model, in one place:
//!
//! - **Stored paths** (node ids, memory locations, governs) are relative to the
//!   project (worktree) root, use `/`, and carry no leading `./` and no
//!   trailing `/` ([`normalize_node_path`]).
//! - **Absolute paths of either platform are recognized on any host**
//!   ([`is_absolute_any`]): a daemon on Linux can be handed a Windows path by
//!   its client and must not mistake it for a relative one. `\` and `/` are
//!   both separators; the Win32 verbatim (`\\?\`) and device (`\\.\`) prefixes
//!   are dropped.
//! - **Comparison** ([`relative_to`], [`relative_to_on_disk`]): Windows
//!   spellings (drive letter or UNC) compare case-insensitively, POSIX
//!   spellings compare exactly; when spellings differ but both paths exist,
//!   their canonical forms are compared (symlinked roots such as macOS `/var`
//!   vs `/private/var`, 8.3 short names, case on a case-insensitive volume).
//! - **Project root of a file** ([`to_project_relative`]): the caller's
//!   `project_root`, else the nearest ancestor holding `.git` (a directory, or
//!   the FILE of a linked worktree), else the namespace's registered root.
//!   `.NeuroStrata` is never a root marker: `~/.NeuroStrata` exists on daemon
//!   hosts and would root every file under home at the home folder.
//! - **A root argument** ([`validate_root`]) must be absolute on this host,
//!   exist, and be a directory; it comes back canonical (`dunce` on Windows,
//!   so `C:\x` and never `\\?\C:\x`; `std::fs::canonicalize` elsewhere).
//!   Drive-relative `C:foo`, rooted-without-drive `\foo` and Git Bash `/c/x`
//!   spellings are refused with a hint instead of resolving against the
//!   daemon's working directory.
//! - **Source keys** ([`source_key`]) keep their historical form
//!   (`std::fs::canonicalize`, `\\?\` on Windows): they are opaque keys stored
//!   in generation pointers, and changing the spelling would orphan them.

use std::path::{Component, Path, PathBuf};

// ---------------------------------------------------------------------------
// Stored-path spelling
// ---------------------------------------------------------------------------

/// Node ids are paths, and a memory written by an agent names a file the way a
/// human does: `src/parser/ingest.rs`. The walker yields a platform path with
/// backslashes and a leading `./` on Windows, and an edge only forms when the
/// two strings match exactly, so every id goes through here first.
pub fn normalize_node_path(path: &str) -> String {
    let forward = path.replace('\\', "/");
    let trimmed = forward.strip_prefix("./").unwrap_or(&forward);
    trimmed.trim_end_matches('/').to_string()
}

/// The stable key identifying the source (worktree) being ingested: the
/// canonical path from `std::fs::canonicalize`, or the path as given when it
/// does not exist. On Windows that is the verbatim `\\?\C:\...` form. Kept
/// byte-for-byte as it always was -- generation pointers are stored under it.
pub fn source_key(dir_path: &Path) -> String {
    std::fs::canonicalize(dir_path)
        .unwrap_or_else(|_| dir_path.to_path_buf())
        .to_string_lossy()
        .to_string()
}

/// `path` with `.` dropped and `..` applied, or `None` when it is absolute or
/// climbs above its start. For joining a stored relative location onto a root
/// without letting it escape the root.
pub fn normalize_relative(path: &Path) -> Option<PathBuf> {
    let mut normalized = PathBuf::new();
    let mut depth = 0usize;
    for component in path.components() {
        match component {
            Component::Normal(part) => {
                normalized.push(part);
                depth += 1;
            }
            Component::CurDir => {}
            Component::ParentDir if depth > 0 => {
                normalized.pop();
                depth -= 1;
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(normalized)
}

/// Whether a stored location is absolute on this host or on any other: a
/// drive path (`C:\`, `C:/`), anything starting with `\\`, or a rooted POSIX
/// path. A rooted POSIX location stored by a Linux peer is absolute data on a
/// Windows host even though the OS calls it rooted-but-relative. Broader than
/// [`is_absolute_any`] on purpose: it is the export's "do not join this onto a
/// root" test, where a malformed `\\x` must not be joined either.
pub fn is_absolute_location(location: &str) -> bool {
    if Path::new(location).is_absolute() {
        return true;
    }
    let bytes = location.as_bytes();
    let windows_drive_absolute = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\');
    let windows_unc_absolute = bytes.starts_with(b"\\\\");
    let posix_absolute = bytes.first() == Some(&b'/');
    windows_drive_absolute || windows_unc_absolute || posix_absolute
}

// ---------------------------------------------------------------------------
// Absolute paths of either platform
// ---------------------------------------------------------------------------

/// A lexically parsed absolute path: its root token and its components.
#[derive(Debug, Clone, PartialEq, Eq)]
struct AbsPath {
    /// Windows paths (drive letter or UNC) compare case-insensitively, as the
    /// file systems they live on do by default.
    windows: bool,
    /// `C:`, `//server/share`, or `/`.
    root: String,
    parts: Vec<String>,
}

impl AbsPath {
    fn to_path_string(&self) -> String {
        let mut out = self.root.clone();
        if self.windows {
            out.push('/');
        }
        out.push_str(&self.parts.join("/"));
        out
    }
}

/// Parse `raw` as an absolute path of either platform, or `None` when it is
/// relative. Backslashes count as separators on every host, as they already do
/// in [`normalize_node_path`]. The Win32 verbatim (`\\?\`) and device (`\\.\`)
/// prefixes are dropped, so `\\?\C:\x` and `C:\x` are the same path.
fn parse_absolute(raw: &str) -> Option<AbsPath> {
    let forward = raw.replace('\\', "/");
    let mut rest: &str = &forward;
    let mut unc = false;
    for verbatim in ["//?/", "//./"] {
        if let Some(after) = rest.strip_prefix(verbatim) {
            if let Some(share) = after
                .strip_prefix("UNC/")
                .or_else(|| after.strip_prefix("unc/"))
            {
                rest = share;
                unc = true;
            } else {
                rest = after;
            }
            break;
        }
    }

    let bytes = rest.as_bytes();
    let (windows, root, tail): (bool, String, &str) = if !unc
        && bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes.len() == 2 || bytes[2] == b'/')
    {
        (
            true,
            format!("{}:", (bytes[0] as char).to_ascii_uppercase()),
            rest.get(2..).unwrap_or(""),
        )
    } else if unc || rest.starts_with("//") {
        let mut segments = rest.trim_start_matches('/').splitn(3, '/');
        let server = segments.next().filter(|s| !s.is_empty())?;
        let share = segments.next().filter(|s| !s.is_empty())?;
        let tail = segments.next().unwrap_or("");
        (
            true,
            format!("//{}/{}", server.to_lowercase(), share.to_lowercase()),
            tail,
        )
    } else if rest.starts_with('/') {
        (false, "/".to_string(), rest)
    } else {
        return None;
    };

    let mut parts: Vec<String> = Vec::new();
    for part in tail.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            p => parts.push(p.to_string()),
        }
    }
    Some(AbsPath { windows, root, parts })
}

/// Whether `raw` is an absolute path of either platform: a drive path, a UNC
/// path, a verbatim `\\?\` path, or a rooted POSIX path. `C:foo` (drive
/// relative) is not absolute.
pub fn is_absolute_any(raw: &str) -> bool {
    parse_absolute(raw).is_some()
}

fn same_component(windows: bool, a: &str, b: &str) -> bool {
    if windows {
        a == b || a.to_lowercase() == b.to_lowercase()
    } else {
        a == b
    }
}

/// `path` relative to `root`, by spelling alone, with `/` separators; `None`
/// unless `root` strictly contains `path`. Windows spellings compare
/// case-insensitively; POSIX spellings compare exactly.
pub fn relative_to(path: &str, root: &str) -> Option<String> {
    let path = parse_absolute(path)?;
    let root = parse_absolute(root)?;
    if path.windows != root.windows || path.parts.len() <= root.parts.len() {
        return None;
    }
    let same_root = if path.windows {
        path.root.eq_ignore_ascii_case(&root.root)
    } else {
        path.root == root.root
    };
    if !same_root {
        return None;
    }
    let prefix_matches = path
        .parts
        .iter()
        .zip(root.parts.iter())
        .all(|(a, b)| same_component(path.windows, a, b));
    if !prefix_matches {
        return None;
    }
    Some(path.parts[root.parts.len()..].join("/"))
}

/// The host's own form of an absolute path, or `None` when this host would
/// read it as relative (a `C:\` path on Linux, say). Probing the disk with a
/// path the host reads as relative would probe the daemon's working directory.
fn host_path(path: &str) -> Option<PathBuf> {
    let parsed = parse_absolute(path)?;
    let host = PathBuf::from(parsed.to_path_string());
    host.is_absolute().then_some(host)
}

/// Canonical form via `dunce`, which on Windows returns `C:\x` rather than the
/// `\\?\C:\x` that `std::fs::canonicalize` gives.
fn canonical(path: &Path) -> Option<PathBuf> {
    dunce::canonicalize(path).ok()
}

/// [`relative_to`], retried on the canonical forms when the spellings differ
/// but both paths exist on this host: a symlinked root (`/var` vs
/// `/private/var`), a different case on a case-insensitive volume, an 8.3
/// short name. A file that does not exist yet (about to be written) is
/// canonicalized through its nearest existing ancestor.
pub fn relative_to_on_disk(path: &str, root: &str) -> Option<String> {
    if let Some(rel) = relative_to(path, root) {
        return Some(rel);
    }
    let root_canonical = canonical(&host_path(root)?)?;
    let host = host_path(path)?;
    let mut existing = host.as_path();
    let mut missing: Vec<String> = Vec::new();
    let base = loop {
        if let Some(c) = canonical(existing) {
            break c;
        }
        missing.push(existing.file_name()?.to_string_lossy().to_string());
        existing = existing.parent()?;
    };
    let mut full = base;
    for part in missing.iter().rev() {
        full.push(part);
    }
    relative_to(&full.to_string_lossy(), &root_canonical.to_string_lossy())
}

// ---------------------------------------------------------------------------
// Project roots
// ---------------------------------------------------------------------------

/// The nearest ancestor of `path` holding `.git` (a directory, or the file of
/// a linked worktree or submodule) -- the first pass of the plugins'
/// `findProjectRoot`. Returned in the input's own spelling, so [`relative_to`]
/// can strip it. `None` when the path is not absolute on this host or no
/// ancestor is a git checkout. `.NeuroStrata` is not a marker.
pub fn find_git_root(path: &str) -> Option<String> {
    let parsed = parse_absolute(path)?;
    host_path(path)?;
    for depth in (0..parsed.parts.len()).rev() {
        let candidate = AbsPath {
            windows: parsed.windows,
            root: parsed.root.clone(),
            parts: parsed.parts[..depth].to_vec(),
        };
        let candidate = candidate.to_path_string();
        if Path::new(&candidate).join(".git").exists() {
            return Some(candidate);
        }
    }
    None
}

/// Whether `worktree` is a linked git worktree of the repository at `root`:
/// its `.git` is a file whose `gitdir:` points inside `root/.git`.
pub fn is_linked_worktree_of(worktree: &str, root: &str) -> bool {
    let dot_git = Path::new(worktree).join(".git");
    if !dot_git.is_file() {
        return false;
    }
    let Ok(text) = std::fs::read_to_string(&dot_git) else {
        return false;
    };
    let Some(gitdir) = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("gitdir:"))
        .map(str::trim)
    else {
        return false;
    };
    // `worktree.useRelativePaths` (git 2.48+) writes the gitdir relative to
    // the worktree.
    let gitdir = if is_absolute_any(gitdir) {
        gitdir.to_string()
    } else {
        Path::new(worktree).join(gitdir).to_string_lossy().to_string()
    };
    let root_git = format!("{}/.git", root.replace('\\', "/").trim_end_matches('/'));
    relative_to_on_disk(&gitdir, &root_git).is_some()
}

/// The git root of a DIRECTORY: `dir` itself when it holds `.git`, else its
/// nearest ancestor that does ([`find_git_root`] looks only above its input).
pub fn git_root_of_dir(dir: &str) -> Option<String> {
    if host_path(dir).is_some_and(|d| d.join(".git").exists()) {
        Some(dir.to_string())
    } else {
        find_git_root(dir)
    }
}

/// The main checkout of the git repository holding `dir` (a directory): `dir`'s
/// own `.git` root when that is a regular checkout, or, for a linked worktree
/// (whose `.git` is a file with `gitdir: <main>/.git/worktrees/<name>`), the
/// checkout that owns it. A submodule or a worktree of a bare repository has no
/// main checkout to borrow from and answers its own root. `None` outside git.
/// Returned in this host's plain spelling (`dunce`).
pub fn main_checkout_root(dir: &str) -> Option<PathBuf> {
    let own_root = git_root_of_dir(dir)?;
    let own = host_path(&own_root)?;
    let dot_git = own.join(".git");
    if dot_git.is_dir() {
        return canonical(&own);
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let gitdir = text.lines().find_map(|l| l.trim().strip_prefix("gitdir:")).map(str::trim)?;
    let gitdir = if is_absolute_any(gitdir) {
        gitdir.to_string()
    } else {
        own.join(gitdir).to_string_lossy().to_string()
    };
    let parsed = parse_absolute(&gitdir)?;
    let main = parsed
        .parts
        .windows(2)
        .position(|w| w[0] == ".git" && w[1] == "worktrees")
        .map(|at| AbsPath { parts: parsed.parts[..at].to_vec(), ..parsed.clone() });
    match main {
        Some(main) => canonical(&host_path(&main.to_path_string())?),
        None => canonical(&own),
    }
}

/// Where the root that relativized a path came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootSource {
    /// The path was already relative and is used exactly as given.
    Relative,
    /// The caller's `project_root` argument.
    ProjectRoot,
    /// The nearest ancestor holding `.git` (directory or worktree file).
    Marker,
    /// The project root cached on the namespace's registry row.
    RegistryRoot,
    /// Absolute, but under no root this namespace knows: left unchanged.
    Outside,
}

impl RootSource {
    pub fn as_str(self) -> &'static str {
        match self {
            RootSource::Relative => "relative",
            RootSource::ProjectRoot => "project_root",
            RootSource::Marker => "marker",
            RootSource::RegistryRoot => "registry_root",
            RootSource::Outside => "outside",
        }
    }
}

/// A caller's path made project-relative, and how.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LookupPath {
    /// What the store is queried with (it still goes through
    /// [`normalize_node_path`]). For `Relative` and `Outside` this is the input
    /// byte for byte.
    pub path: String,
    pub source: RootSource,
    /// The root `path` is relative to, in the spelling it was found in; `None`
    /// for `Relative` and `Outside`.
    pub root: Option<String>,
}

fn depth(rel: &str) -> usize {
    rel.split('/').count()
}

/// Turn the path a caller sent into the project-relative path the graph
/// stores. A relative path is returned byte for byte, without touching the
/// disk. An absolute one is made relative to, in order:
/// 1. `project_root`, when it contains the path -- the caller named it;
/// 2. the nearest ancestor holding `.git`, so a file in a linked worktree
///    under `.claude/worktrees/agent-x/` resolves against that worktree, whose
///    layout mirrors the repository;
/// 3. `registry_root`, the namespace's registered project root.
///
/// When the namespace has a registered root, a `.git` root that is neither
/// inside it, around it, nor a linked worktree of it belongs to another
/// repository: the path is left unchanged (`Outside`) instead of borrowing
/// this namespace's memories for a file with the same relative name. Between a
/// registered root and a `.git` root that both contain the file, the deeper
/// one wins (a project registered below its repository's `.git`).
///
/// When the file exists, the part below the root is taken from its canonical
/// path, so it carries the on-disk case: the store matches exactly, and a
/// Windows caller may spell `SRC\LIB.RS`. A relative path is never rewritten,
/// case included.
pub fn to_project_relative(
    raw: &str,
    project_root: Option<&str>,
    registry_root: Option<&str>,
) -> LookupPath {
    if !is_absolute_any(raw) {
        return LookupPath { path: raw.to_string(), source: RootSource::Relative, root: None };
    }
    match choose_root(raw, project_root, registry_root) {
        Some((root, rel, source)) => LookupPath {
            path: with_disk_spelling(raw, &root, rel),
            source,
            root: Some(root),
        },
        None => LookupPath { path: raw.to_string(), source: RootSource::Outside, root: None },
    }
}

/// `rel` (the part of `path` below `root`) re-read from the filesystem when
/// the file exists: the canonical path carries the on-disk case and resolves
/// symlinked roots. Falls back to `rel` when the file does not exist yet, or
/// its canonical path leaves the root (a symlink pointing out of the project).
fn with_disk_spelling(path: &str, root: &str, rel: String) -> String {
    let (Some(file), Some(root_host)) = (host_path(path), host_path(root)) else {
        return rel;
    };
    let (Some(file), Some(root_c)) = (canonical(&file), canonical(&root_host)) else {
        return rel;
    };
    relative_to(&file.to_string_lossy(), &root_c.to_string_lossy()).unwrap_or(rel)
}

/// The root that relativizes absolute `raw`, the part below it, and where the
/// root came from; `None` when no root this namespace knows contains it.
fn choose_root(
    raw: &str,
    project_root: Option<&str>,
    registry_root: Option<&str>,
) -> Option<(String, String, RootSource)> {
    if let Some(root) = project_root {
        if let Some(rel) = relative_to_on_disk(raw, root) {
            return Some((root.to_string(), rel, RootSource::ProjectRoot));
        }
    }
    let registry = registry_root
        .and_then(|root| relative_to_on_disk(raw, root).map(|rel| (root.to_string(), rel)));
    if let Some(marker) = find_git_root(raw) {
        if let Some(marker_rel) = relative_to(raw, &marker) {
            let by_marker = Some((marker.clone(), marker_rel.clone(), RootSource::Marker));
            match (registry_root, &registry) {
                (None, _) => return by_marker,
                // Both contain the file: the deeper root is the more specific
                // one. A worktree inside the registered checkout is deeper; a
                // project registered below the repository's `.git` is deeper.
                (Some(_), Some((reg_root, reg_rel))) => {
                    return if depth(reg_rel) < depth(&marker_rel) {
                        Some((reg_root.clone(), reg_rel.clone(), RootSource::RegistryRoot))
                    } else {
                        by_marker
                    };
                }
                // A worktree checked out beside the main checkout.
                (Some(root), None) if is_linked_worktree_of(&marker, root) => return by_marker,
                // Another repository's file: answering with its relative path
                // would hand back this namespace's memories for a stranger.
                (Some(_), None) => {}
            }
        }
    }
    registry.map(|(root, rel)| (root, rel, RootSource::RegistryRoot))
}

// ---------------------------------------------------------------------------
// Root arguments
// ---------------------------------------------------------------------------

/// Why `raw` cannot name a project root on a host of the given kind, judged by
/// spelling alone; `None` when the spelling is fine. Host-independent so both
/// kinds are tested on every host.
fn root_form_error(raw: &str, windows_host: bool) -> Option<String> {
    if raw.trim().is_empty() {
        return Some("project root is empty; pass the absolute path of the project directory".into());
    }
    let bytes = raw.as_bytes();
    let drive_relative = bytes.len() >= 2
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && !(bytes.len() >= 3 && matches!(bytes[2], b'/' | b'\\'));
    if drive_relative {
        return Some(format!(
            "project root `{raw}` is drive-relative (no separator after `{}:`), so it would \
             resolve against the daemon's working directory; did a shell strip the \
             backslashes? Pass `{}:\\...` or `{}:/...`",
            bytes[0] as char, bytes[0] as char, bytes[0] as char
        ));
    }
    let msys_drive = bytes.len() >= 2
        && bytes[0] == b'/'
        && bytes[1].is_ascii_alphabetic()
        && (bytes.len() == 2 || bytes[2] == b'/');
    if windows_host && msys_drive {
        return Some(format!(
            "project root `{raw}` is a Git Bash (MSYS) path; this daemon runs on Windows and \
             reads it as `\\{}`. Pass the Windows form `{}:/{}` (Git Bash: `pwd -W` or \
             `cygpath -m`)",
            &raw[1..],
            (bytes[1] as char).to_ascii_uppercase(),
            raw.get(3..).unwrap_or("")
        ));
    }
    match parse_absolute(raw) {
        None => Some(format!(
            "project root `{raw}` is relative, so it would resolve against the daemon's \
             working directory; pass an absolute path"
        )),
        Some(parsed) if parsed.windows && !windows_host => Some(format!(
            "project root `{raw}` is a Windows path, but this daemon does not run on Windows"
        )),
        Some(parsed) if !parsed.windows && windows_host => Some(format!(
            "project root `{raw}` has no drive letter; on Windows a rooted path without one \
             resolves against the daemon's current drive. Pass `C:\\...`"
        )),
        Some(_) => None,
    }
}

/// Check a project-root argument and return its canonical form: absolute on
/// this host, existing, a directory. Windows roots come back plain (`C:\x`,
/// on-disk case, no `\\?\` where a plain form exists); other hosts get
/// `std::fs::canonicalize` (realpath, so macOS `/var` becomes `/private/var`).
/// Nothing here falls back to the working directory.
pub fn validate_root(raw: &str) -> Result<PathBuf, String> {
    if let Some(error) = root_form_error(raw, cfg!(windows)) {
        return Err(error);
    }
    let path = Path::new(raw);
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => return Err(format!("project root `{raw}` is not a directory")),
        Err(e) => return Err(format!("project root `{raw}` cannot be read: {e}")),
    }
    dunce::canonicalize(path).map_err(|e| format!("project root `{raw}` cannot be resolved: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn got(p: &LookupPath) -> (&str, RootSource) {
        (p.path.as_str(), p.source)
    }

    // ---- normalize_node_path / source_key / locations (pure) ----

    #[test]
    fn node_paths_use_forward_slashes_without_dot_or_trailing_slash() {
        assert_eq!(normalize_node_path(r".\src\lib.rs"), "src/lib.rs");
        assert_eq!(normalize_node_path("./src/lib.rs"), "src/lib.rs");
        assert_eq!(normalize_node_path("src/lib.rs"), "src/lib.rs");
        assert_eq!(normalize_node_path("src/"), "src");
        assert_eq!(normalize_node_path(r"src\"), "src");
        assert_eq!(normalize_node_path(""), "");
    }

    #[test]
    fn source_key_of_a_missing_path_is_the_path_as_given() {
        let missing = Path::new("/nonexistent-ns-paths/example-project");
        assert_eq!(source_key(missing), missing.to_string_lossy());
    }

    #[test]
    fn source_key_keeps_the_std_canonical_form() {
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(
            source_key(tmp.path()),
            std::fs::canonicalize(tmp.path()).unwrap().to_string_lossy()
        );
    }

    #[test]
    fn stored_locations_are_absolute_in_either_platform_spelling() {
        for location in [r"C:\x\a.rs", "c:/x/a.rs", r"\\server\share\a.rs", "/home/me/a.rs", r"\\x"] {
            assert!(is_absolute_location(location), "{location}");
        }
        for location in ["src/lib.rs", r"src\lib.rs", "C:lib.rs", "C:", "./a", ""] {
            assert!(!is_absolute_location(location), "{location}");
        }
    }

    #[test]
    fn normalize_relative_never_leaves_its_start() {
        assert_eq!(normalize_relative(Path::new("a/./b/../c")), Some(PathBuf::from("a/c")));
        assert_eq!(normalize_relative(Path::new("../a")), None);
        assert_eq!(normalize_relative(Path::new("a/../../b")), None);
        assert_eq!(normalize_relative(Path::new("/a")), None);
    }

    // ---- to_project_relative: pure, host-independent ----

    #[test]
    fn relative_input_is_returned_byte_for_byte() {
        for raw in ["src/lib.rs", r"src\lib.rs", "./src/lib.rs", "lib.rs", "C:lib.rs", ""] {
            let p = to_project_relative(raw, Some("/home/me/example-project"), Some(r"C:\x"));
            assert_eq!(p, LookupPath { path: raw.to_string(), source: RootSource::Relative, root: None });
        }
    }

    #[test]
    fn absolute_forms_of_both_platforms_are_recognized() {
        for raw in [
            r"C:\dev\example-project\src\lib.rs",
            "c:/dev/example-project/src/lib.rs",
            r"\\?\C:\dev\example-project\src\lib.rs",
            r"\\server\share\example-project\src\lib.rs",
            r"\\?\UNC\server\share\example-project\src\lib.rs",
            "/home/me/example-project/src/lib.rs",
        ] {
            assert!(is_absolute_any(raw), "{raw}");
        }
        for raw in ["src/lib.rs", r"src\lib.rs", "C:lib.rs", "./a", "../a"] {
            assert!(!is_absolute_any(raw), "{raw}");
        }
    }

    #[test]
    fn windows_paths_strip_case_insensitively_across_separators_and_verbatim() {
        let root = r"C:\dev\projects\example-project\platform";
        for path in [
            r"C:\dev\projects\example-project\platform\renderer\lightpanda_process.py",
            "c:/Dev/Projects/Example-Project/platform/renderer/lightpanda_process.py",
            r"\\?\C:\dev\projects\example-project\platform\renderer\lightpanda_process.py",
        ] {
            assert_eq!(
                relative_to(path, root).as_deref(),
                Some("renderer/lightpanda_process.py"),
                "{path}"
            );
        }
        assert_eq!(relative_to(r"C:\dev\a\b.rs", r"\\?\c:\dev\").as_deref(), Some("a/b.rs"));
        assert_eq!(
            relative_to(
                r"\\?\UNC\Server\Share\example-project\src\lib.rs",
                r"\\server\share\example-project"
            )
            .as_deref(),
            Some("src/lib.rs")
        );
        // Another drive, a sibling with a shared name prefix, the root itself.
        assert_eq!(relative_to(r"D:\dev\a.rs", r"C:\dev"), None);
        assert_eq!(relative_to(r"C:\dev\projects2\a.rs", r"C:\dev\projects"), None);
        assert_eq!(relative_to(r"C:\dev\projects", r"C:\dev\projects"), None);
    }

    #[test]
    fn posix_paths_strip_exactly() {
        let root = "/home/me/example-project";
        assert_eq!(
            relative_to("/home/me/example-project/src/lib.rs", root).as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(
            relative_to("/home/me/example-project/./src/../src/lib.rs", "/home/me/example-project/")
                .as_deref(),
            Some("src/lib.rs")
        );
        // POSIX names are case-sensitive; a case difference is left to the
        // on-disk fallback, which only a real volume can answer.
        assert_eq!(relative_to("/home/me/Example-Project/src/lib.rs", root), None);
        assert_eq!(relative_to("/home/me/example-project2/src/lib.rs", root), None);
        // Mixed platforms never match.
        assert_eq!(relative_to("/c/dev/a.rs", r"C:\dev"), None);
        assert_eq!(relative_to(r"C:\home\me\a.rs", "/home/me"), None);
    }

    #[test]
    fn malformed_shell_spellings_never_match_a_windows_root() {
        // Git Bash strips unquoted backslashes (`C:devprojectsx`) and spells
        // drives `/c/...`; neither may resolve against a registered root.
        let root = r"C:\dev\projects\example-project";
        for raw in [r"C:devprojectsexample-projectsrclib.rs", "/c/dev/projects/example-project/src/lib.rs"] {
            let p = to_project_relative(raw, Some(root), Some(root));
            assert!(
                matches!(p.source, RootSource::Relative | RootSource::Outside),
                "{raw} -> {p:?}"
            );
            assert_eq!(p.path, raw);
        }
    }

    #[test]
    fn caller_project_root_wins_when_it_contains_the_path() {
        let p = to_project_relative(
            r"C:\dev\projects\example-project\platform\renderer\x.py",
            Some(r"C:\dev\projects\example-project\platform"),
            Some(r"C:\dev\projects\example-project"),
        );
        assert_eq!(got(&p), ("renderer/x.py", RootSource::ProjectRoot));
        assert_eq!(p.root.as_deref(), Some(r"C:\dev\projects\example-project\platform"));
    }

    #[test]
    fn registry_root_strips_a_path_with_no_marker_on_this_host() {
        // A Windows path asked of a POSIX daemon, or a path that does not
        // exist here: no marker can be probed, the registered root still names
        // the project.
        let p = to_project_relative(
            "/nonexistent-ns-lookup/example-project/src/lib.rs",
            None,
            Some("/nonexistent-ns-lookup/example-project"),
        );
        assert_eq!(got(&p), ("src/lib.rs", RootSource::RegistryRoot));
        assert_eq!(p.root.as_deref(), Some("/nonexistent-ns-lookup/example-project"));
        let p = to_project_relative(
            r"Q:\nonexistent-ns-lookup\example-project\src\lib.rs",
            None,
            Some(r"q:/nonexistent-ns-lookup/example-project/"),
        );
        assert_eq!(got(&p), ("src/lib.rs", RootSource::RegistryRoot));
    }

    #[test]
    fn an_absolute_path_under_no_known_root_is_left_unchanged() {
        let raw = "/nonexistent-ns-lookup/elsewhere/src/lib.rs";
        let p = to_project_relative(raw, Some("/nonexistent-ns-lookup/example-project"), None);
        assert_eq!(p, LookupPath { path: raw.into(), source: RootSource::Outside, root: None });
    }

    // ---- root arguments: pure, both host kinds on every host ----

    #[test]
    fn windows_root_spellings_are_judged_for_a_windows_host() {
        for ok in [r"C:\dev\x", "c:/dev/x", r"\\?\C:\dev\x", r"\\server\share\x", r"C:\"] {
            assert_eq!(root_form_error(ok, true), None, "{ok}");
        }
        for (bad, hint) in [
            ("C:devprojectsx", "drive-relative"),
            ("C:", "drive-relative"),
            ("/c/dev/x", "Git Bash"),
            ("/c", "Git Bash"),
            (r"\dev\x", "no drive letter"),
            ("/home/me/x", "no drive letter"),
            ("dev/x", "relative"),
            ("~/x", "relative"),
            ("", "empty"),
            ("   ", "empty"),
        ] {
            let error = root_form_error(bad, true).unwrap_or_else(|| panic!("{bad} accepted"));
            assert!(error.contains(hint), "{bad}: {error}");
        }
        assert!(root_form_error("/c/dev/x", true).unwrap().contains("C:/dev/x"));
    }

    #[test]
    fn posix_root_spellings_are_judged_for_a_posix_host() {
        for ok in ["/home/me/x", "/c/dev/x", "/", "/var/folders/x"] {
            assert_eq!(root_form_error(ok, false), None, "{ok}");
        }
        for (bad, hint) in [
            ("rel/x", "relative"),
            ("~", "relative"),
            ("./x", "relative"),
            ("", "empty"),
            (r"C:\dev\x", "Windows path"),
            ("C:devx", "drive-relative"),
        ] {
            let error = root_form_error(bad, false).unwrap_or_else(|| panic!("{bad} accepted"));
            assert!(error.contains(hint), "{bad}: {error}");
        }
    }

    #[test]
    fn validate_root_refuses_missing_paths_and_files() {
        let tmp = tempfile::tempdir().unwrap();
        let base = canonical(tmp.path()).unwrap();
        let missing = base.join("missing");
        assert!(validate_root(&missing.to_string_lossy()).unwrap_err().contains("cannot be read"));
        let file = base.join("file.txt");
        std::fs::write(&file, b"x").unwrap();
        assert!(validate_root(&file.to_string_lossy()).unwrap_err().contains("not a directory"));
        assert_eq!(validate_root(&base.to_string_lossy()).unwrap(), base);
    }

    #[cfg(windows)]
    #[test]
    fn validate_root_returns_the_plain_on_disk_spelling_on_windows() {
        let tmp = tempfile::tempdir().unwrap();
        let base = canonical(tmp.path()).unwrap().join("Example-Project");
        std::fs::create_dir_all(&base).unwrap();
        let plain = base.to_string_lossy().to_string();
        for given in [
            plain.to_uppercase(),
            format!(r"\\?\{plain}"),
            plain.replace('\\', "/"),
            format!(r"{plain}\"),
        ] {
            assert_eq!(validate_root(&given).unwrap(), base, "{given}");
        }
        let error = validate_root(&plain.replace('\\', "")).unwrap_err();
        assert!(error.contains("drive-relative"), "{error}");
    }

    #[cfg(unix)]
    #[test]
    fn validate_root_resolves_a_symlinked_root_on_posix() {
        let tmp = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(tmp.path()).unwrap().join("real-project");
        std::fs::create_dir_all(&real).unwrap();
        let link = std::fs::canonicalize(tmp.path()).unwrap().join("linked-project");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(validate_root(&link.to_string_lossy()).unwrap(), real);
    }

    // ---- filesystem layouts (temp dirs) ----

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"x").unwrap();
    }

    /// `<tmp>/example-project` with a `.git` directory, `src/lib.rs`, and a
    /// linked worktree at `.claude/worktrees/agent-1` whose `.git` is a file.
    fn project_with_worktree() -> (tempfile::TempDir, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = canonical(tmp.path()).unwrap().join("example-project");
        std::fs::create_dir_all(root.join(".git").join("worktrees").join("agent-1")).unwrap();
        touch(&root.join("src").join("lib.rs"));
        let worktree = root.join(".claude").join("worktrees").join("agent-1");
        touch(&worktree.join("src").join("lib.rs"));
        let gitdir = root.join(".git").join("worktrees").join("agent-1");
        std::fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", gitdir.to_string_lossy().replace('\\', "/")),
        )
        .unwrap();
        (tmp, root, worktree)
    }

    fn s(p: &Path) -> String {
        p.to_string_lossy().to_string()
    }

    #[test]
    fn marker_walk_finds_the_git_root_without_any_root_argument() {
        let (_tmp, root, _) = project_with_worktree();
        let file = root.join("src").join("lib.rs");
        let p = to_project_relative(&s(&file), None, None);
        assert_eq!(got(&p), ("src/lib.rs", RootSource::Marker));
        // A file about to be created (does not exist yet) resolves the same way.
        let new_file = root.join("src").join("new").join("mod.rs");
        assert_eq!(to_project_relative(&s(&new_file), None, None).path, "src/new/mod.rs");
    }

    #[test]
    fn a_worktree_file_resolves_against_the_worktree_not_the_main_checkout() {
        let (_tmp, root, worktree) = project_with_worktree();
        let file = s(&worktree.join("src").join("lib.rs"));
        // No registry root, and with the main checkout registered: both give
        // the repository-relative path, never `.claude/worktrees/agent-1/...`.
        for registry in [None, Some(s(&root))] {
            let p = to_project_relative(&file, None, registry.as_deref());
            assert_eq!(got(&p), ("src/lib.rs", RootSource::Marker));
        }
    }

    #[test]
    fn a_worktree_beside_the_main_checkout_is_recognized_by_its_gitdir() {
        let (tmp, root, _) = project_with_worktree();
        let beside = canonical(tmp.path()).unwrap().join("example-project-wt");
        touch(&beside.join("src").join("lib.rs"));
        std::fs::create_dir_all(root.join(".git").join("worktrees").join("wt")).unwrap();
        std::fs::write(
            beside.join(".git"),
            format!("gitdir: {}\n", s(&root.join(".git").join("worktrees").join("wt"))),
        )
        .unwrap();
        let p = to_project_relative(&s(&beside.join("src").join("lib.rs")), None, Some(&s(&root)));
        assert_eq!(got(&p), ("src/lib.rs", RootSource::Marker));
    }

    #[test]
    fn another_repository_is_not_answered_for_when_the_namespace_has_a_root() {
        let (tmp, root, _) = project_with_worktree();
        let other = canonical(tmp.path()).unwrap().join("other-repo");
        std::fs::create_dir_all(other.join(".git")).unwrap();
        let file = s(&other.join("src").join("lib.rs"));
        touch(Path::new(&file));
        let p = to_project_relative(&file, None, Some(&s(&root)));
        assert_eq!(p, LookupPath { path: file.clone(), source: RootSource::Outside, root: None });
    }

    #[test]
    fn a_project_registered_below_the_git_root_wins_over_the_git_root() {
        // Repository at `repo`, NeuroStrata project (and ingest root) at
        // `repo/platform`: stored paths are relative to `platform`.
        let tmp = tempfile::tempdir().unwrap();
        let repo = canonical(tmp.path()).unwrap().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let platform = repo.join("platform");
        let file = platform.join("renderer").join("x.py");
        touch(&file);
        let p = to_project_relative(&s(&file), None, Some(&s(&platform)));
        assert_eq!(got(&p), ("renderer/x.py", RootSource::RegistryRoot));
    }

    #[test]
    fn a_neurostrata_folder_alone_is_not_a_project_root() {
        // `~/.NeuroStrata` exists on machines running the daemon; treating it
        // as a marker would root every file under home at the home folder.
        let tmp = tempfile::tempdir().unwrap();
        let home = canonical(tmp.path()).unwrap().join("home");
        std::fs::create_dir_all(home.join(".NeuroStrata")).unwrap();
        let file = home.join("scratch").join("notes.py");
        touch(&file);
        let above_has_git = std::iter::successors(Some(home.as_path()), |p| p.parent())
            .any(|p| p.join(".git").exists());
        if !above_has_git {
            let p = to_project_relative(&s(&file), None, None);
            assert_eq!(got(&p), (s(&file).as_str(), RootSource::Outside));
        }
    }

    #[test]
    fn the_main_checkout_of_a_linked_worktree_is_the_checkout_that_owns_it() {
        let (tmp, root, worktree) = project_with_worktree();
        assert_eq!(main_checkout_root(&s(&root)), Some(root.clone()));
        assert_eq!(main_checkout_root(&s(&root.join("src"))), Some(root.clone()));
        assert_eq!(main_checkout_root(&s(&worktree)), Some(root.clone()));
        assert_eq!(main_checkout_root(&s(&worktree.join("src"))), Some(root.clone()));
        // A submodule's gitdir is <super>/.git/modules/<name>: its own root.
        let sub = canonical(tmp.path()).unwrap().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join(".git"), "gitdir: ../example-project/.git/modules/sub\n").unwrap();
        assert_eq!(main_checkout_root(&s(&sub)), Some(sub.clone()));
        let outside = canonical(tmp.path()).unwrap().join("plain");
        std::fs::create_dir_all(&outside).unwrap();
        let above_has_git = std::iter::successors(Some(outside.as_path()), |p| p.parent())
            .any(|p| p.join(".git").exists());
        if !above_has_git {
            assert_eq!(main_checkout_root(&s(&outside)), None);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_root_matches_through_its_canonical_form() {
        // macOS: /var/folders/... is /private/var/folders/...; the same shape
        // is built here with an explicit symlink so Linux exercises it too.
        let tmp = tempfile::tempdir().unwrap();
        let real = canonical(tmp.path()).unwrap().join("real-project");
        let file = real.join("src").join("lib.rs");
        touch(&file);
        let link = canonical(tmp.path()).unwrap().join("linked-project");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let p = to_project_relative(&s(&file), Some(&s(&link)), None);
        assert_eq!(got(&p), ("src/lib.rs", RootSource::ProjectRoot));
    }

    #[cfg(windows)]
    #[test]
    fn a_windows_root_spelled_in_another_case_or_verbatim_matches() {
        let (_tmp, root, _) = project_with_worktree();
        let file = s(&root.join("src").join("lib.rs"));
        let upper = s(&root).to_uppercase();
        let verbatim = format!(r"\\?\{}", s(&root));
        for given in [upper, verbatim] {
            let p = to_project_relative(&file, Some(&given), None);
            assert_eq!(got(&p), ("src/lib.rs", RootSource::ProjectRoot), "{given}");
        }
    }

    /// Windows, and macOS on its default case-insensitive volume: a file
    /// spelled in another case resolves to the on-disk spelling the store
    /// holds. Runs wherever the temp volume is case-insensitive.
    #[test]
    fn a_file_spelled_in_another_case_resolves_to_the_on_disk_case() {
        let (_tmp, root, _) = project_with_worktree();
        let shouted_root = PathBuf::from(s(&root).to_uppercase());
        if !shouted_root.join("SRC").join("LIB.RS").exists() {
            return; // a case-sensitive volume: nothing to resolve
        }
        let shouted = s(&shouted_root.join("SRC").join("LIB.RS"));
        let p = to_project_relative(&shouted, None, None);
        assert_eq!(got(&p), ("src/lib.rs", RootSource::Marker));
    }
}
