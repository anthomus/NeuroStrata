//! Keeps a directory ingest running when the caller that asked for it leaves.
//!
//! Ingestion used to run inside the request handler. axum drops that future the
//! moment the client disconnects, so a caller that timed out, or a window that
//! was closed, took the walk down with it -- part-built, before `relink_edges`,
//! and therefore with every GOVERNS edge missing. Nothing recorded that it had
//! happened: the daemon stayed healthy and idle, and the only symptom was a
//! graph that looked wrong (bead neurostrata-7ej).
//!
//! The walk now belongs to a task of its own. Requests wait on it and report
//! what it did, but leaving no longer stops it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::watch;

use crate::parser::ingest::IngestObserver;
use crate::parser::schema::ParserSchema;
use crate::traits::{Embedder, VectorStore};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IngestState {
    Running,
    Finished,
    Failed,
}

/// What a walk has done so far. Reported while it runs, and kept afterwards so
/// asking about a namespace tells you how its last ingest ended.
#[derive(Clone, Debug, Serialize)]
pub struct IngestProgress {
    pub namespace: String,
    pub dir: String,
    pub state: IngestState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Every file the walk has finished with. Moves through directories that
    /// hold no symbols, which is what tells a watcher the walk is alive.
    pub files_seen: usize,
    /// The subset that produced at least one symbol.
    pub files_ingested: usize,
    pub symbols_ingested: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relinked_edges: Option<usize>,
    pub started_unix: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub finished_unix: Option<u64>,
}

impl IngestProgress {
    fn started(namespace: &str, dir: &str) -> Self {
        Self {
            namespace: namespace.to_string(),
            dir: dir.to_string(),
            state: IngestState::Running,
            error: None,
            files_seen: 0,
            files_ingested: 0,
            symbols_ingested: 0,
            last_file: None,
            relinked_edges: None,
            started_unix: now_unix(),
            finished_unix: None,
        }
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Turns the walk's callbacks into something a waiting request can read.
struct ProgressReporter {
    tx: watch::Sender<IngestProgress>,
}

impl IngestObserver for ProgressReporter {
    fn file_walked(&self, path: &str, symbols: usize) {
        self.tx.send_modify(|progress| {
            // Every file moves this and last_file, so a watcher can tell a walk
            // crossing a directory of markdown from one that has stopped.
            progress.files_seen += 1;
            progress.last_file = Some(path.to_string());

            if symbols > 0 {
                progress.files_ingested += 1;
                progress.symbols_ingested += symbols;
            }
        });
    }

    fn relinked(&self, edges: usize) {
        self.tx.send_modify(|progress| progress.relinked_edges = Some(edges));
    }
}

/// One walk per namespace, owned here rather than by whoever asked for it.
#[derive(Default)]
pub struct IngestJobs {
    /// Keyed by namespace. An entry outlives its walk, which costs one receiver
    /// per namespace and keeps the check below a single lookup.
    jobs: Mutex<HashMap<String, watch::Receiver<IngestProgress>>>,
}

impl IngestJobs {
    pub fn new() -> Self {
        Self::default()
    }

    /// What the last walk of this namespace is doing, or did.
    ///
    /// `None` when this daemon has not been asked to ingest that namespace
    /// since it started -- which is not the same as never having ingested it,
    /// since the registry lives in memory and the graph lives on disk.
    pub fn progress(&self, namespace: &str) -> Option<IngestProgress> {
        self.jobs
            .lock()
            .unwrap()
            .get(namespace)
            .map(|rx| rx.borrow().clone())
    }

    /// Starts the walk without waiting for it, and reports what it is doing.
    ///
    /// The caller gets an answer in the time it takes to register a job rather
    /// than in the time it takes to embed a repository, and reads `progress`
    /// afterwards. Attaching to a walk already under way is the same operation:
    /// what comes back describes the live one either way.
    pub fn start(
        self: &Arc<Self>,
        namespace: &str,
        dir: &str,
        schema: ParserSchema,
        embedder: Arc<dyn Embedder>,
        vector_store: Arc<dyn VectorStore>,
    ) -> IngestProgress {
        let rx = self.start_or_attach(namespace, dir, schema, embedder, vector_store);
        let progress = rx.borrow().clone();
        progress
    }

    /// Runs the ingest and waits for it, or waits for the one already running.
    ///
    /// Two walks over one namespace must never overlap: they would each be
    /// rewriting the same rows, and the orphan sweep at the end of one would
    /// see the other's half-finished tree. A caller that gave up and retried
    /// used to produce exactly that pair.
    pub async fn run(
        self: &Arc<Self>,
        namespace: &str,
        dir: &str,
        schema: ParserSchema,
        embedder: Arc<dyn Embedder>,
        vector_store: Arc<dyn VectorStore>,
    ) -> anyhow::Result<IngestProgress> {
        let mut rx = self.start_or_attach(namespace, dir, schema, embedder, vector_store);

        loop {
            // Cloned out rather than held: the borrow guard must not be alive
            // across the await below.
            let progress = rx.borrow_and_update().clone();
            match progress.state {
                IngestState::Finished => return Ok(progress),
                IngestState::Failed => {
                    return Err(anyhow::anyhow!(progress
                        .error
                        .unwrap_or_else(|| "ingestion failed".to_string())))
                }
                IngestState::Running => {}
            }

            if rx.changed().await.is_err() {
                // The task went away without reporting, which a panic inside it
                // would do. Say so rather than report success.
                return Err(anyhow::anyhow!(
                    "the ingest of namespace '{}' ended without reporting a result",
                    namespace
                ));
            }
        }
    }

    /// Registers a walk for this namespace, or hands back the one already
    /// running. The single place a walk is spawned, so `start` and `run` cannot
    /// disagree about when a second one is allowed.
    fn start_or_attach(
        self: &Arc<Self>,
        namespace: &str,
        dir: &str,
        schema: ParserSchema,
        embedder: Arc<dyn Embedder>,
        vector_store: Arc<dyn VectorStore>,
    ) -> watch::Receiver<IngestProgress> {
        {
            let mut jobs = self.jobs.lock().unwrap();

            let already_running = jobs
                .get(namespace)
                .is_some_and(|rx| rx.borrow().state == IngestState::Running);

            if already_running {
                jobs.get(namespace).expect("checked just above").clone()
            } else {
                let (tx, rx) = watch::channel(IngestProgress::started(namespace, dir));
                jobs.insert(namespace.to_string(), rx.clone());

                let namespace = namespace.to_string();
                let dir = dir.to_string();

                // Detached deliberately. tokio::spawn keeps running when its
                // JoinHandle is dropped, which is what makes a disconnect stop
                // mattering: the request goes, the walk does not.
                tokio::spawn(async move {
                    let reporter: Arc<dyn IngestObserver> =
                        Arc::new(ProgressReporter { tx: tx.clone() });

                    let outcome = crate::parser::ingest::ingest_directory(
                        std::path::Path::new(&dir),
                        &schema,
                        embedder,
                        vector_store,
                        &namespace,
                        Some(reporter),
                    )
                    .await;

                    tx.send_modify(|progress| {
                        progress.finished_unix = Some(now_unix());
                        match &outcome {
                            Ok(()) => progress.state = IngestState::Finished,
                            Err(e) => {
                                progress.state = IngestState::Failed;
                                progress.error = Some(e.to_string());
                            }
                        }
                    });
                });

                rx
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn progress(state: IngestState) -> IngestProgress {
        let mut progress = IngestProgress::started("ns", "dir");
        progress.state = state;
        progress
    }

    /// A walk that is still running is what a second caller attaches to, so the
    /// state a live entry reports has to be readable while it runs.
    #[test]
    fn a_live_walk_reports_that_it_is_running() {
        let jobs = IngestJobs::new();
        let (tx, rx) = watch::channel(progress(IngestState::Running));
        jobs.jobs.lock().unwrap().insert("ns".to_string(), rx);

        let running = |jobs: &IngestJobs| {
            jobs.jobs
                .lock()
                .unwrap()
                .get("ns")
                .is_some_and(|rx| rx.borrow().state == IngestState::Running)
        };

        assert!(running(&jobs));

        tx.send_modify(|progress| {
            progress.state = IngestState::Failed;
            progress.error = Some("no such directory".to_string());
        });

        // Finished, so the next caller starts a walk rather than attaching to
        // one that has already ended.
        assert!(!running(&jobs));
    }

    /// The registry lives in memory. A namespace this daemon has not been asked
    /// to ingest has no job, which a caller must be able to tell apart from one
    /// that is still running -- the route answers 404 on this.
    #[test]
    fn a_namespace_with_no_job_has_no_progress() {
        let jobs = IngestJobs::new();

        assert!(jobs.progress("never-ingested").is_none());
    }

    /// What a caller polls instead of holding the connection open: the live
    /// counters, readable while the walk is still going.
    #[test]
    fn progress_reads_the_live_walk() {
        let jobs = IngestJobs::new();
        let (tx, rx) = watch::channel(progress(IngestState::Running));
        jobs.jobs.lock().unwrap().insert("ns".to_string(), rx);

        tx.send_modify(|progress| {
            progress.files_ingested = 12;
            progress.symbols_ingested = 340;
        });

        let seen = jobs.progress("ns").expect("the job was just inserted");
        assert_eq!(seen.state, IngestState::Running);
        assert_eq!(seen.files_ingested, 12);
        assert_eq!(seen.symbols_ingested, 340);

        // Still readable once it ends: that is how a poller learns it finished.
        tx.send_modify(|progress| progress.state = IngestState::Finished);
        assert_eq!(
            jobs.progress("ns").expect("the entry outlives its walk").state,
            IngestState::Finished
        );
    }

    /// Progress is what a caller reads instead of waiting, so the counts have
    /// to accumulate across files rather than report only the last one.
    #[test]
    fn progress_accumulates_over_the_walk() {
        let (tx, rx) = watch::channel(progress(IngestState::Running));
        let reporter = ProgressReporter { tx };

        reporter.file_walked("src/a.rs", 3);
        reporter.file_walked("src/b.rs", 4);
        reporter.relinked(20);

        let progress = rx.borrow();
        assert_eq!(progress.files_seen, 2);
        assert_eq!(progress.files_ingested, 2);
        assert_eq!(progress.symbols_ingested, 7);
        assert_eq!(progress.last_file.as_deref(), Some("src/b.rs"));
        assert_eq!(progress.relinked_edges, Some(20));
    }

    /// The bug this guards: a walk crossing markdown, JSON and configs reported
    /// nothing at all, so a healthy ingest looked hung and the counter stopped
    /// being evidence of anything (bead neurostrata-hit).
    #[test]
    fn a_file_with_no_symbols_still_moves_the_walk() {
        let (tx, rx) = watch::channel(progress(IngestState::Running));
        let reporter = ProgressReporter { tx };

        reporter.file_walked("README.md", 0);
        reporter.file_walked("package.json", 0);

        let progress = rx.borrow();
        assert_eq!(progress.files_seen, 2, "the walk moved and must say so");
        assert_eq!(progress.last_file.as_deref(), Some("package.json"));

        // And it must not claim symbols it did not find.
        assert_eq!(progress.files_ingested, 0);
        assert_eq!(progress.symbols_ingested, 0);
    }
}
