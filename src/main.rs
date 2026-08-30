mod config;
mod daemon;
mod embed;
mod parser;
mod server;
mod store;
mod traits;

use config::Config;
use embed::FastEmbedder;
use std::sync::Arc;
use crate::traits::SearchResult;
use store::LadybugStore;
use traits::{Embedder, VectorStore};
use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(name = "neurostrata-mcp")]
#[command(about = "🧠 NeuroStrata — MCP Server & CLI Engine", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Start NeuroStrata in daemon-only mode
    Daemon,

    /// List all active namespaces
    Namespaces,

    /// List all memories in a given namespace
    List {
        /// The target namespace
        namespace: String,
    },

    /// Ingest a directory of code symbols into a namespace
    Ingest {
        /// The directory path to ingest
        dir: String,

        /// The target namespace
        namespace: String,

        /// Optional path to the parser schema JSON file
        schema_path: Option<String>,
    },

    /// Export the memory graph to a JSON file
    #[command(name = "export-graph")]
    ExportGraph {
        /// Output path for the JSON graph export
        out_path: Option<String>,
    },

    /// Delete a memory from a namespace by ID
    Delete {
        /// The target namespace
        namespace: String,

        /// The memory ID to delete
        id: String,
    },

    /// Add a new memory to a namespace
    Add {
        /// The target namespace
        namespace: String,

        /// The memory type (e.g. symbol, text)
        memory_type: String,

        /// The memory content string
        content: String,

        /// Optional physical location metadata
        location: Option<String>,
    },

    /// Stop a running daemon so it checkpoints before exiting
    Shutdown,

    /// Report what an upgrade left inconsistent, changing nothing
    Doctor,

    /// Run an external plugin or helper, explicitly
    ///
    /// This used to happen implicitly for any unrecognised subcommand, which
    /// meant a typo executed whatever it named. Ask for it by name instead:
    ///   neurostrata-mcp run my-plugin --flag value
    Run {
        /// The program to execute
        program: String,

        /// Arguments passed to it unchanged
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Write a portable copy of the database to a directory
    Backup {
        /// Directory to write the backup into. Must not already exist
        dir: String,
    },

    /// Rebuild a database from a backup, into a file that does not exist yet
    Restore {
        /// Directory a backup was written to
        dir: String,

        /// Where to build the restored database. Defaults to the configured
        /// db_path, which must not already exist
        #[arg(long)]
        into: Option<String>,
    },

    /// Edit an existing memory
    Edit {
        /// The target namespace
        namespace: String,

        /// The memory ID to edit
        id: String,

        /// The new namespace to move/save to
        new_namespace: String,

        /// The new content
        content: String,

        /// The new location
        location: String,
    },
}

/// What a health probe actually found.
///
/// Silence is the case worth naming. A daemon busy inside the engine answers
/// nothing for minutes at a time, and reporting that as "no daemon is running"
/// sends people hunting a process that is very much alive -- or worse, killing
/// it and losing every write since the last checkpoint. Silence cannot be
/// resolved from here either: on this machine a connection to the port with
/// nothing behind it hangs instead of being refused, so a timeout genuinely
/// means "one of two things". Say that, rather than pick one.
#[derive(Clone, Copy, PartialEq, Debug)]
enum DaemonProbe {
    Responsive,
    Silent,
    Absent,
}

/// Kept separate from the request so the distinction can be tested without a
/// socket. Only an explicit refusal proves absence.
fn classify_probe(reached: bool, refused: bool) -> DaemonProbe {
    if reached {
        DaemonProbe::Responsive
    } else if refused {
        DaemonProbe::Absent
    } else {
        DaemonProbe::Silent
    }
}

async fn probe_daemon() -> DaemonProbe {
    match reqwest::Client::new()
        .get("http://127.0.0.1:34343/health")
        .timeout(std::time::Duration::from_millis(500))
        .send()
        .await
    {
        Ok(_) => classify_probe(true, false),
        Err(e) => classify_probe(false, e.is_connect()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_mistyped_subcommand_is_an_error_not_an_execution() {
        let parsed = Cli::try_parse_from(["neurostrata-mcp", "lsit"]);
        assert!(parsed.is_err(), "a typo must never reach std::process::Command");
    }

    #[test]
    fn running_a_plugin_has_to_be_asked_for_by_name() {
        let cli = Cli::try_parse_from(["neurostrata-mcp", "run", "my-plugin", "--flag", "value"])
            .expect("run takes a program and passes its arguments through");

        match cli.command {
            Some(Commands::Run { program, args }) => {
                assert_eq!(program, "my-plugin");
                assert_eq!(args, vec!["--flag".to_string(), "value".to_string()]);
            }
            other => panic!("expected Run, got {:?}", other),
        }
    }

    #[test]
    fn only_a_refusal_proves_no_daemon() {
        assert_eq!(classify_probe(false, true), DaemonProbe::Absent);
    }

    #[test]
    fn silence_is_not_reported_as_an_absent_daemon() {
        assert_eq!(classify_probe(false, false), DaemonProbe::Silent);
    }

    #[test]
    fn an_answered_probe_is_a_live_daemon() {
        assert_eq!(classify_probe(true, false), DaemonProbe::Responsive);
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // Check if the daemon is already running on port 34343
    let probe = probe_daemon().await;
    // Only a daemon that answered gets to hold the database lock as far as the
    // CLI is concerned. Treating silence as "running" would refuse every local
    // command on a machine where an empty port times out rather than refuses.
    let daemon_running = probe == DaemonProbe::Responsive;

    // If no arguments, start standard MCP stdio mode
    if args.len() == 1 {
        if daemon_running {
            eprintln!("Daemon is already running. Starting MCP proxy...");
            server::start_mcp_proxy(server::DaemonOrigin::AlreadyRunning).await?;
            return Ok(());
        } else {
            eprintln!("NeuroStrata MCP Server initializing...");
            
            // Spawn the daemon as a detached process
            let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("neurostrata-mcp"));
            std::process::Command::new(exe)
                .arg("daemon")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()?;
            
            eprintln!("Waiting for daemon to become ready (this may take a moment while models load)...");
            
            // Wait for daemon to become ready. Bounded per attempt as well as
            // overall: an unbound port on this machine hangs rather than
            // refusing -- the behaviour DaemonProbe exists to describe -- so an
            // untimed send here could spend the whole "30 seconds max" inside
            // one call.
            let client = reqwest::Client::new();
            for _ in 0..300 { // 30 seconds max
                if client
                    .get("http://127.0.0.1:34343/health")
                    .timeout(std::time::Duration::from_millis(500))
                    .send()
                    .await
                    .is_ok()
                {
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            }

            // Deliberately not conditional on that loop having succeeded. A
            // first run downloads the embedding model and takes far longer than
            // 30s, and the proxy is told where the daemon came from so it waits
            // rather than reporting a daemon it started itself as absent.
            server::start_mcp_proxy(server::DaemonOrigin::SpawnedByUs).await?;
            return Ok(());
        }
    }

    // An unrecognised first argument used to be run as an external program.
    // That made every typo an execution: `neurostrata-mcp lsit` ran whatever
    // `lsit` resolved to on PATH, and anything that could put a file on the
    // PATH could therefore have it invoked by a mistyped memory command. The
    // capability now needs asking for, by name, through `run` (bead
    // neurostrata-7s8). clap reports anything else as the unknown subcommand
    // it is.

    // Now parse using clap
    let cli = Cli::parse();

    if let Some(command) = cli.command {
        match command {
            Commands::Daemon => {
                println!("NeuroStrata MCP Server initializing in DAEMON-ONLY mode...");
                let config = Config::from_default_path()?;
                let embedder = Arc::new(FastEmbedder::new()?);
                let vector_store: Arc<dyn VectorStore> = Arc::new(LadybugStore::new(
                    config.db_path.to_string_lossy().to_string(),
                    embedder.dimensions(),
                )?);
                vector_store.init("global").await?;
                daemon::start_daemon(embedder, vector_store).await?;
            }
            Commands::Run { program, args } => {
                // Deliberate, named, and it never touches the database.
                match std::process::Command::new(&program).args(&args).status() {
                    Ok(status) => std::process::exit(status.code().unwrap_or(1)),
                    Err(e) => {
                        eprintln!("Could not run '{}': {}", program, e);
                        std::process::exit(1);
                    }
                }
            }
            Commands::Shutdown => {
                match probe {
                    DaemonProbe::Absent => {
                        println!("No daemon is running on 127.0.0.1:34343.");
                        return Ok(());
                    }
                    DaemonProbe::Silent => {
                        eprintln!("Nothing answered on 127.0.0.1:34343 within 500ms. Either no daemon is running, or one is busy in the database and cannot answer yet -- those look identical from here. Sending a stop request and waiting; if a daemon is there, this can take a couple of minutes. Do not kill it.");
                    }
                    DaemonProbe::Responsive => {}
                }

                let client = reqwest::Client::new();
                // A busy daemon may not answer this either. That is not a
                // failure: the request is queued, so fall through to the wait.
                if let Err(e) = client
                    .post("http://127.0.0.1:34343/shutdown")
                    .timeout(std::time::Duration::from_secs(10))
                    .send()
                    .await
                {
                    if e.is_connect() {
                        println!("Nothing is listening on 127.0.0.1:34343 now -- either a daemon stopped as this ran, or there was never one to stop.");
                        return Ok(());
                    }
                    eprintln!("The stop request has not been acknowledged yet: {}. Waiting for the daemon to go anyway.", e);
                }

                // Wait for it to actually go: the checkpoint happens after the
                // HTTP response, and a CLI command run too early hits the lock.
                // A daemon that was already wedged gets longer, because engine
                // waits of two and a half minutes have been measured.
                let attempts = if probe == DaemonProbe::Responsive { 300 } else { 2400 };
                for _ in 0..attempts {
                    let still_up = client
                        .get("http://127.0.0.1:34343/health")
                        .timeout(std::time::Duration::from_millis(500))
                        .send()
                        .await
                        .is_ok();
                    if !still_up {
                        println!("Daemon stopped and checkpointed.");
                        return Ok(());
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                }
                eprintln!(
                    "The daemon was still listening after {} seconds. It is probably still finishing a database operation -- leave it, and do not kill it: writes since the last checkpoint would be lost.",
                    attempts / 10
                );
                std::process::exit(1);
            }
            Commands::Backup { dir } => {
                // The database is single-writer. When a daemon holds it, ask the
                // daemon to do the work rather than fighting it for the lock.
                if daemon_running {
                    let res = reqwest::Client::new()
                        .post("http://127.0.0.1:34343/backup")
                        .json(&serde_json::json!({ "dir": dir }))
                        .send()
                        .await?;
                    let status = res.status();
                    let body = res.text().await.unwrap_or_default();
                    if !status.is_success() {
                        eprintln!("Backup failed: {}", body);
                        std::process::exit(1);
                    }
                    println!("{}", body);
                    return Ok(());
                }

                let config = Config::from_default_path()?;
                let embedder = Arc::new(FastEmbedder::new()?);
                let vector_store: Arc<dyn VectorStore> = Arc::new(LadybugStore::new(
                    config.db_path.to_string_lossy().to_string(),
                    embedder.dimensions(),
                )?);
                vector_store.export_database(&dir).await?;
                println!("Backed up to {}", dir);
                return Ok(());
            }
            Commands::Restore { dir, into } => {
                // IMPORT DATABASE replays the exported schema, so it only works
                // against a database that has none. Restoring therefore builds a
                // new file rather than overwriting a live one -- nothing existing
                // is dropped, and the switch stays a deliberate step.
                let config = Config::from_default_path()?;
                let target = into
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| config.db_path.clone());

                if target.exists() {
                    eprintln!("{:?} already exists, and restoring into it would mean replacing what it holds.", target);
                    eprintln!("Restore into a new path instead: neurostrata-mcp restore <backup-dir> --into <new-db-path>");
                    eprintln!("Then point db_path in ~/.config/neurostrata/config.json at it once you have checked it.");
                    std::process::exit(1);
                }
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)?;
                }

                let embedder = Arc::new(FastEmbedder::new()?);
                let vector_store: Arc<dyn VectorStore> = Arc::new(LadybugStore::new(
                    target.to_string_lossy().to_string(),
                    embedder.dimensions(),
                )?);
                // Deliberately no init() here: the backup carries its own schema.
                vector_store.import_database(&dir).await?;

                let namespaces = vector_store.list_namespaces().await.unwrap_or_default();
                println!("Restored {} into {:?}", dir, target);
                if !namespaces.is_empty() {
                    println!("It holds {} namespace(s): {}", namespaces.len(), namespaces.join(", "));
                }
                if target != config.db_path {
                    println!("To use it, set db_path in ~/.config/neurostrata/config.json to {:?}", target);
                }
                return Ok(());
            }
            other => {
                if daemon_running {
                    eprintln!("CRITICAL ERROR: The NeuroStrata daemon is currently running (likely via OpenCode) and holds the database lock.");
                    eprintln!("You cannot run database-modifying CLI commands while the daemon is active.");
                    eprintln!("Run `neurostrata-mcp shutdown` to stop it safely -- killing the process discards any writes made since the last checkpoint.");
                    std::process::exit(1);
                }
                
                let config = Config::from_default_path()?;
                let embedder = Arc::new(FastEmbedder::new()?);
                let vector_store: Arc<dyn VectorStore> = Arc::new(LadybugStore::new(
                    config.db_path.to_string_lossy().to_string(),
                    embedder.dimensions(),
                )?);

                match other {
                    Commands::Doctor => {
                        // Read-only by design: it names what an upgrade left
                        // behind and how to fix it, and touches nothing itself.
                        let namespaces = vector_store.list_namespaces().await?;
                        println!("Namespaces: {:?}\n", namespaces);

                        let mut collisions = 0;
                        for (i, a) in namespaces.iter().enumerate() {
                            for b in namespaces.iter().skip(i + 1) {
                                if a.eq_ignore_ascii_case(b) {
                                    collisions += 1;
                                    let a_len = vector_store.list(a, None).await.map(|m| m.len()).unwrap_or(0);
                                    let b_len = vector_store.list(b, None).await.map(|m| m.len()).unwrap_or(0);
                                    println!("Two spellings of one project:");
                                    println!("  '{}' holds {} memories", a, a_len);
                                    println!("  '{}' holds {} memories", b, b_len);
                                    let (from, to) = if a_len < b_len { (a, b) } else { (b, a) };
                                    println!(
                                        "  Merge with neurostrata_move_memory, moving each id from '{}' into '{}'.\n",
                                        from, to
                                    );
                                }
                            }
                        }
                        if collisions == 0 {
                            println!("No namespaces differ only by case.\n");
                        }

                        for ns in &namespaces {
                            let memories = vector_store.list(ns, None).await?;
                            let known = crate::store::ladybug::KnownIds::new(
                                memories.iter().map(|m| m.id.as_str()),
                            );

                            let mut resolvable = Vec::new();
                            let mut missing = Vec::new();
                            let mut never_read = 0;

                            for memory in &memories {
                                if memory.payload.metadata.get("access_count").and_then(|v| v.as_i64()).unwrap_or(0) == 0 {
                                    never_read += 1;
                                }
                                for edge in crate::store::ladybug::edge_specs(&memory.payload.metadata) {
                                    if known.contains(&edge.target_id) {
                                        continue;
                                    }
                                    match known.resolve(&edge.target_id) {
                                        Some(_) => resolvable.push(edge.target_id.clone()),
                                        None => missing.push(edge.target_id.clone()),
                                    }
                                }
                            }

                            println!("{}: {} memories", ns, memories.len());
                            println!(
                                "  declared targets that need the older absolute form resolved: {}",
                                resolvable.len()
                            );
                            for target in resolvable.iter().take(3) {
                                println!("    {}", target);
                            }
                            println!("  declared targets that match nothing ingested: {}", missing.len());
                            for target in missing.iter().take(3) {
                                println!("    {}", target);
                            }
                            println!(
                                "  memories never counted as read: {} of {}",
                                never_read,
                                memories.len()
                            );
                            if never_read == memories.len() && !memories.is_empty() {
                                println!(
                                    "    Every one. Before the embedding decode was fixed, each retrieval tried to"
                                );
                                println!(
                                    "    write an empty vector and was refused, so the Neural Gain Filter had nothing"
                                );
                                println!("    to rank by. Counts start rising again from the next search.");
                            }
                            println!();
                        }
                    }
                    Commands::Namespaces => {
                        let namespaces = vector_store.list_namespaces().await?;
                        println!("Namespaces:");
                        for ns in namespaces {
                            println!("  - {}", ns);
                        }
                    }
                    Commands::List { namespace } => {
                        let results: Vec<SearchResult> = vector_store.list(&namespace, None).await?;
                        println!("Found {} memories in namespace '{}':\n", results.len(), namespace);
                        for res in results {
                            let location_str = if res.payload.location.is_empty() {
                                "N/A".to_string()
                            } else {
                                res.payload.location.clone()
                            };
                            println!("--- ID: {} ---", res.id);
                            println!("Type: {}", res.payload.memory_type);
                            println!("Location: {}", location_str);
                            println!("Content: {}\n", res.payload.content);
                        }
                    }
                    Commands::Ingest { dir, namespace, schema_path } => {
                        let dir_path = std::path::Path::new(&dir);
                        let schema_str = if let Some(path) = schema_path {
                            std::fs::read_to_string(&path).unwrap_or_else(|e| {
                                eprintln!("Failed to read schema from {}: {}", path, e);
                                std::process::exit(1);
                            })
                        } else {
                            include_str!("schema.json").to_string()
                        };

                        if let Ok(schema) = crate::parser::schema::ParserSchema::load(&schema_str) {
                            println!("Ingesting AST from {:?} into namespace '{}'", dir_path, namespace);
                            crate::parser::ingest::ingest_directory(dir_path, &schema, embedder, vector_store, &namespace).await?;
                            println!("Ingestion complete.");
                        }
                    }
                    Commands::ExportGraph { out_path } => {
                        let default_path = ".NeuroStrata/graph/graph.json".to_string();
                        let target_path = out_path.as_ref().unwrap_or(&default_path);
                        println!("Exporting Memory Graph to {}", target_path);
                        if let Some(parent) = std::path::Path::new(target_path).parent() {
                            std::fs::create_dir_all(parent)?;
                        }
                        vector_store.init("global").await?;
                        let graph_data = vector_store.export_graph().await?;
                        std::fs::write(target_path, serde_json::to_string_pretty(&graph_data)?)?;
                        println!("Graph exported successfully.");
                    }
                    Commands::Delete { namespace, id } => {
                        vector_store.delete(&namespace, &id).await?;
                        println!("Memory deleted successfully.");
                    }
                    Commands::Add { namespace, memory_type, content, location } => {
                        let vector = embedder.embed(&content).await?;
                        let payload = crate::traits::MemoryPayload {
                            content: content.clone(),
                            memory_type: memory_type.clone(),
                            location: location.unwrap_or_default(),
                            user_id: "system".to_string(),
                            agent_name: Some("NeuroStrata".to_string()),
                            location_lines: "".to_string(),
                            metadata: serde_json::json!({}),
                        };
                        let id = uuid::Uuid::new_v4().to_string();
                        vector_store.upsert(&namespace, &id, vector, payload).await?;
                        println!("Memory added successfully with ID: {}", id);
                    }
                    Commands::Edit { namespace, id, new_namespace, content, location } => {
                        if let Some((_, mut payload)) = vector_store.get(&namespace, &id).await? {
                            vector_store.delete(&namespace, &id).await?;
                            payload.content = content.clone();
                            payload.location = location.clone();
                            let vector = embedder.embed(&content).await?;
                            vector_store.upsert(&new_namespace, &id, vector, payload).await?;
                            println!("Successfully edited memory {}", id);
                        }
                    }
                    // Handled above, before the database is ever opened.
                    Commands::Daemon
                    | Commands::Shutdown
                    | Commands::Run { .. }
                    | Commands::Backup { .. }
                    | Commands::Restore { .. } => unreachable!(),
                }
            }
        }
    }

    Ok(())
}
