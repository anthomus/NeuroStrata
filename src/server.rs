use crate::traits::{Embedder, MemoryPayload, VectorStore};
use serde::{Deserialize, Serialize};
use serde_json::{self, Value};
use std::sync::Arc;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Deserialize)]
pub struct JsonRpcRequest {
    #[allow(dead_code)]
    jsonrpc: String,
    pub id: Option<Value>,
    pub method: String,
    #[allow(dead_code)]
    pub params: Option<Value>,
}

#[derive(Serialize)]
pub struct JsonRpcResponse<T> {
    jsonrpc: String,
    id: Option<Value>,
    result: Option<T>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

impl<T> JsonRpcResponse<T> {
    pub fn success(id: Option<Value>, result: T) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: Some(result),
            error: None,
        }
    }
}

#[allow(dead_code)]
impl JsonRpcResponse<Value> {
    pub fn error(id: Option<Value>, error: Value) -> Self {
        Self {
            jsonrpc: "2.0".to_string(),
            id,
            result: None,
            error: Some(error),
        }
    }
}

pub async fn process_mcp_request(
    request: JsonRpcRequest,
    emb: Arc<dyn Embedder>,
    store: Arc<dyn VectorStore>,
) -> Value {
    let id = request.id.clone();
    match request.method.as_str() {
        "initialize" => {
            let result = serde_json::json!({
                "protocolVersion": "2024-11-05",
                "serverInfo": {
                    "name": "neurostrata-mcp",
                    "version": "1.0.0"
                },
                "capabilities": {
                    "tools": {}
                }
            });
            serde_json::to_value(JsonRpcResponse::success(id.clone(), result)).unwrap_or_else(|e| {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32603,
                        "message": format!("Internal serialization error: {}", e)
                    }
                })
            })
        }
        "notifications/initialized" => {
            serde_json::json!({})
        }
        "tools/list" => {
            let result = serde_json::json!({
                "tools": [
                    {
                        "name": "neurostrata_add_memory",
                        "description": "Store an architectural rule, project pattern, or task insight.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "content": { "type": "string", "description": "The text of the memory to save." },
                                "namespace": { "type": "string", "description": "The exact project name (e.g., 'NeuroStrata') or 'global'. Do not use folder paths." },
                                "project_root": { "type": "string", "description": "The absolute path to the project root directory where the agent is currently working." },
                                "memory_type": { "type": "string", "description": "Type of memory: 'rule', 'preference', 'bootstrap', 'persona', or 'context'. Defaults to 'context'." },
                                "create_new_namespace": { "type": "boolean", "description": "Set to true ONLY if you are absolutely certain this is a brand new project namespace that doesn't exist yet." },
                                "user_id": { "type": "string", "description": "The user making the request." },
                                "agent_name": { "type": "string", "description": "The name of the agent storing the memory." },
                                "locations": { 
                                    "type": "array", 
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "path": { "type": "string", "description": "File path (e.g. docs/architecture.md)" },
                                            "lines": { "type": "string", "description": "Line numbers (e.g. 42-49)" },
                                            "symbol": { "type": "string", "description": "Code symbol (e.g. startSync())" }
                                        }
                                    },
                                    "description": "An array of file paths, line numbers, and symbols this memory governs. Memories MUST reference the specific documents they belong to."
                                },
                                "domain": { "type": "string", "description": "Optional category or domain this rule belongs to (e.g., 'frontend', 'database', 'devops', 'api')." },
                                "related_to": { "type": "array", "items": { "type": "string" }, "description": "Optional list of memory IDs this rule connects to, forming a knowledge graph edge." },
                                "metadata": { "type": "object", "description": "Optional dictionary with Bi-Directional Anchors" }
                            },
                            "required": ["content", "namespace", "project_root"]
                        }
                    },
                    {
                        "name": "neurostrata_edit_memory",
                        "description": "Correct an existing memory in place. Use this instead of adding a second memory when a rule has evolved or was stored with a mistake, so the namespace keeps one coherent answer. Read the memory first with neurostrata_get_memory.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string", "description": "The id of the memory to edit, as returned by neurostrata_search_memory or neurostrata_get_memory." },
                                "namespace": { "type": "string", "description": "The namespace the memory lives in. The memory is not moved; use neurostrata_move_memory for that." },
                                "content": { "type": "string", "description": "Replacement text. The memory is re-embedded when this changes, so pass the whole new text rather than a fragment." },
                                "memory_type": { "type": "string", "description": "Optional new type: 'rule', 'preference', 'bootstrap', 'persona', or 'context'." },
                                "location": { "type": "string", "description": "Optional new primary file path this memory governs." },
                                "location_lines": { "type": "string", "description": "Optional new line range (e.g. 42-49)." },
                                "domain": { "type": "string", "description": "Optional new domain (e.g., 'frontend', 'database', 'devops', 'api')." },
                                "allow_global": { "type": "boolean", "description": "Required to be true before anything in the 'global' namespace can be edited, because those rules apply to every project." }
                            },
                            "required": ["id", "namespace"]
                        }
                    },
                    {
                        "name": "neurostrata_get_memory",
                        "description": "Fetch a single memory by id. Use it to read a memory before editing it, and to follow a Related Nodes or Governs pointer to the exact record instead of guessing at it with a search.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string", "description": "The id of the memory to fetch." },
                                "namespace": { "type": "string", "description": "The namespace the memory lives in." }
                            },
                            "required": ["id", "namespace"]
                        }
                    },
                    {
                        "name": "neurostrata_delete_memory",
                        "description": "Delete one memory by id. Use it to prune a hallucinated or obsolete Engram you put there. Prefer neurostrata_edit_memory when the record should survive in corrected form; deletion is for records that should never have existed.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string", "description": "The id of the memory to delete, as returned by neurostrata_search_memory or neurostrata_get_memory." },
                                "namespace": { "type": "string", "description": "The namespace the memory lives in." },
                                "allow_global": { "type": "boolean", "description": "Required to be true before anything in the 'global' namespace can be deleted, because those rules apply to every project." }
                            },
                            "required": ["id", "namespace"]
                        }
                    },
                    {
                        "name": "neurostrata_get_graph",
                        "description": "Walk the memory graph. Given an id, returns what that node is connected to and how (CONTAINS for structure, GOVERNS for a rule over code, RELATES_TO for a semantic link); without one, returns a summary of the namespace's shape. Use it to find the rule governing a file, or the file a symbol belongs to.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "namespace": { "type": "string", "description": "The namespace to walk." },
                                "id": { "type": "string", "description": "Optional node to start from. Ids of code nodes are normalised paths, e.g. 'src/store/ladybug.rs'. Omit for a summary of the whole namespace." },
                                "depth": { "type": "integer", "description": "How many hops to follow from the starting node. Defaults to 1, capped at 3." }
                            },
                            "required": ["namespace"]
                        }
                    },
                    {
                        "name": "neurostrata_list_memories",
                        "description": "List what a namespace holds, without a similarity query. Use it to audit or de-duplicate -- noticing that four Engrams cover the same component is impossible through search alone.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "namespace": { "type": "string", "description": "The namespace to list." },
                                "memory_type": { "type": "string", "description": "Optional filter: 'rule', 'context', 'bootstrap', 'code_ast', 'file', 'directory', and so on." },
                                "limit": { "type": "integer", "description": "How many to return. Defaults to 50." }
                            },
                            "required": ["namespace"]
                        }
                    },
                    {
                        "name": "neurostrata_get_snapshot",
                        "description": "Get a pre-computed cognitive snapshot of the most important active architectural rules for a project. Use this immediately upon starting a new task to ground yourself in the project's core architecture before searching.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "namespace": { "type": "string", "description": "The exact project name (e.g., 'NeuroStrata') or 'global'." }
                            },
                            "required": ["namespace"]
                        }
                    },
                    {
                        "name": "neurostrata_ingest_directory",
                        "description": "Batch ingest and parse the Abstract Syntax Tree (AST) of the current project directory to build the Software Graph.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "dir_path": { "type": "string", "description": "Absolute path to the project directory to ingest. Usually the current working directory." },
                                "namespace": { "type": "string", "description": "The project namespace." }
                            },
                            "required": ["dir_path", "namespace"]
                        }
                    },
                    {
                        "name": "neurostrata_list_namespaces",
                        "description": "List all existing project namespaces in the database. Use this to prevent hallucinating namespace names.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {}
                        }
                    },
                    {
                        "name": "neurostrata_move_memory",
                        "description": "Promote or demote an Engram between strata by moving it between namespaces, keeping its id and its access history. Use it for the Tri-Strata lifecycle (task insight -> project rule -> machine-wide rule); adding a copy instead resets the access count that drives recall ordering and pruning.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "id": { "type": "string", "description": "The id of the memory to move." },
                                "source_namespace": { "type": "string", "description": "The namespace it lives in now." },
                                "target_namespace": { "type": "string", "description": "The namespace it should live in. Must already exist unless create_new_namespace is set." },
                                "project_root": { "type": "string", "description": "Absolute path of the project you are working in. Required unless the target is 'global'." },
                                "create_new_namespace": { "type": "boolean", "description": "Set to true ONLY when the target namespace is genuinely new." },
                                "allow_global": { "type": "boolean", "description": "Required to be true when either end of the move is the 'global' namespace." }
                            },
                            "required": ["id", "source_namespace", "target_namespace"]
                        }
                    },
                    {
                        "name": "neurostrata_search_memory",
                        "description": "Search the project's long-term memory for architectural rules.",
                        "inputSchema": {
                            "type": "object",
                            "properties": {
                                "query": { "type": "string", "description": "What to search for." },
                                "namespace": { "type": "string", "description": "The exact project name (e.g., 'NeuroStrata') or 'global'. Do not use folder paths." }
                            },
                            "required": ["query", "namespace"]
                        }
                    }
                ]
            });
            serde_json::to_value(JsonRpcResponse::success(id.clone(), result)).unwrap_or_else(|e| {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32603,
                        "message": format!("Internal serialization error: {}", e)
                    }
                })
            })
        }
        "tools/call" => {
            let mut result_text = "Tool execution failed".to_string();
            if let Some(params) = &request.params {
                if let Some(name) = params.get("name").and_then(|n| n.as_str()) {
                    let arguments = params
                        .get("arguments")
                        .cloned()
                        .unwrap_or(serde_json::json!({}));

                    match name {
                        "neurostrata_list_namespaces" => {
                            result_text = handle_list_namespaces(store.clone()).await;
                        }
                        "neurostrata_add_memory" => {
                            result_text = handle_add_memory(arguments, emb.clone(), store.clone()).await;
                        }
                        "neurostrata_edit_memory" => {
                            result_text = handle_edit_memory(arguments, emb.clone(), store.clone()).await;
                        }
                        "neurostrata_delete_memory" => {
                            result_text = handle_delete_memory(arguments, store.clone()).await;
                        }
                        "neurostrata_get_graph" => {
                            result_text = handle_get_graph_tool(arguments, store.clone()).await;
                        }
                        "neurostrata_get_memory" => {
                            result_text = handle_get_memory(arguments, store.clone()).await;
                        }
                        "neurostrata_list_memories" => {
                            result_text = handle_list_memories(arguments, store.clone()).await;
                        }
                        "neurostrata_get_snapshot" => {
                            result_text = handle_get_snapshot(arguments, store.clone()).await;
                        }
                        "neurostrata_ingest_directory" => {
                            result_text = handle_ingest_directory(arguments, emb.clone(), store.clone()).await;
                        }
                        "neurostrata_move_memory" => {
                            result_text = handle_move_memory(arguments, store.clone()).await;
                        }
                        "neurostrata_search_memory" => {
                            result_text = handle_search_memory(arguments, emb.clone(), store.clone()).await;
                        }
                        _ => {
                            result_text = format!("Unknown tool: {}", name);
                        }
                    }
                }
            }

            let result = serde_json::json!({
                "content": [
                    { "type": "text", "text": result_text }
                ]
            });
            serde_json::to_value(JsonRpcResponse::success(id.clone(), result)).unwrap_or_else(|e| {
                serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32603,
                        "message": format!("Internal serialization error: {}", e)
                    }
                })
            })
        }
        _ => serde_json::json!({})
    }
}

// Optional proxy helper to keep backwards compat with the stdio loop
pub async fn start_mcp_proxy() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = BufReader::new(stdin).lines();
    let mut writer = stdout;

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(60))
        .build()
        .unwrap();

    while let Some(line) = reader.next_line().await? {
        if let Ok(request) = serde_json::from_str::<serde_json::Value>(&line) {
            match client.post("http://127.0.0.1:34343/mcp").json(&request).send().await {
                Ok(resp) => {
                    if let Ok(text) = resp.text().await {
                        writer.write_all(text.as_bytes()).await?;
                        writer.write_all(b"\n").await?;
                        writer.flush().await?;
                    }
                }
                Err(e) => {
                    eprintln!("Failed to proxy MCP request to daemon: {}", e);
                }
            }
        }
    }
    Ok(())
}


async fn handle_list_namespaces(store: Arc<dyn VectorStore>) -> String {
    if let Ok(namespaces) = store.list_namespaces().await {
        format!("Existing namespaces: {:?}", namespaces)
    } else {
        "Failed to list namespaces.".to_string()
    }
}

// A memory is durable only once checkpointed -- WAL replay restores the catalog
// but not row insertions (bead neurostrata-kug) -- and this surface used to
// checkpoint after every write to close that window. It cost more than it
// bought: the engine waits for all transactions to drain before flushing, so
// under load the checkpoint blocked the writer for minutes and failed anyway
// (bead neurostrata-3fi.6.4). The store now marks itself dirty and the daemon's
// background task does the flushing, which is also what runs at shutdown.

async fn handle_add_memory(arguments: Value, emb: Arc<dyn Embedder>, store: Arc<dyn VectorStore>) -> String {
    let content = match arguments.get("content").and_then(|c| c.as_str()) {
        Some(c) => c,
        None => return "Missing 'content' parameter.".to_string(),
    };

    let secret_regex = regex::Regex::new(r"(?i)(sk-ant-|ghp_|xoxb-|eyjhbg|api_key\s*=|password\s*=|sk-proj-)").unwrap();
    if secret_regex.is_match(content) {
        return "ERROR [SECURITY]: Memory rejected due to sensitive information (e.g., API keys, passwords, or tokens). Please redact the secrets from your request and try storing the memory again.".to_string();
    }

    let namespace = match arguments.get("namespace").and_then(|n| n.as_str()) {
        Some(n) => n,
        None => return "ERROR [NAMESPACE]: 'namespace' is missing. You MUST explicitly provide the specific project namespace. NEVER default to 'global' unless instructed.".to_string(),
    };

    if namespace.contains('/') || namespace.contains('\\') {
        return "ERROR [NAMESPACE]: The namespace cannot be a file path. It must be the exact project name (e.g., 'NeuroStrata'). Do not use slashes.".to_string();
    }

    if namespace != "global" {
        if let Some(project_root) = arguments.get("project_root").and_then(|r| r.as_str()) {
            let ns_dir = std::path::Path::new(project_root).join(".NeuroStrata");
            if !tokio::fs::try_exists(&ns_dir).await.unwrap_or(false) {
                let create_new_namespace = arguments.get("create_new_namespace").and_then(|v| v.as_bool()).unwrap_or(false);
                if !create_new_namespace {
                    return format!("ERROR: No .NeuroStrata directory found at {}. This indicates the project does not have a designated context/namespace yet. Do NOT guess the namespace. Ask the user if they want to initialize this directory as a new context, and if so, call this tool again with create_new_namespace=true.", project_root);
                } else {
                    if let Err(e) = tokio::fs::create_dir_all(&ns_dir).await {
                        return format!("ERROR: Failed to create .NeuroStrata directory: {}", e);
                    }
                }
            }
        }
    }

    let memory_type = arguments.get("memory_type").and_then(|m| m.as_str()).unwrap_or("context");
    let create_new_namespace = arguments.get("create_new_namespace").and_then(|v| v.as_bool()).unwrap_or(false);
    let user_id = arguments.get("user_id").and_then(|u| u.as_str()).unwrap_or("unknown");
    if user_id == "auto-ingestor" {
        // Directory ingestion deletes every row owned by this user_id before it
        // rebuilds, so a memory stored under it would vanish on the next ingest.
        return "ERROR: 'auto-ingestor' is reserved for directory ingestion, and memories stored under it are deleted by the next ingest. Please use a different user_id.".to_string();
    }
    let agent_name = arguments.get("agent_name").and_then(|a| a.as_str()).map(|s| s.to_string());
    let mut location = "".to_string();
    let mut location_lines = "".to_string();
    let mut metadata = arguments.get("metadata").cloned().unwrap_or_else(|| serde_json::json!({}));
    
    if let Some(locations) = arguments.get("locations").and_then(|l| l.as_array()) {
        if let Some(first) = locations.first() {
            location = first.get("path").and_then(|p| p.as_str()).unwrap_or("").to_string();
            location_lines = first.get("lines").and_then(|l| l.as_str()).unwrap_or("").to_string();
        }
        
        if let Some(obj) = metadata.as_object_mut() {
            let refs: Vec<serde_json::Value> = locations.iter().map(|loc| {
                let mut ref_obj = serde_json::Map::new();
                if let Some(path) = loc.get("path").and_then(|p| p.as_str()) {
                    ref_obj.insert("file".to_string(), serde_json::json!(path));
                }
                if let Some(lines) = loc.get("lines").and_then(|l| l.as_str()) {
                    ref_obj.insert("lines".to_string(), serde_json::json!(lines));
                }
                if let Some(sym) = loc.get("symbol").and_then(|s| s.as_str()) {
                    ref_obj.insert("symbol".to_string(), serde_json::json!(sym));
                }
                serde_json::Value::Object(ref_obj)
            }).collect();
            obj.insert("refs".to_string(), serde_json::Value::Array(refs));

            // A rule that names files governs them. This is the edge that makes an
            // architectural memory reachable from the code it constrains, rather
            // than only from a similar-sounding query.
            let governed: Vec<serde_json::Value> = locations
                .iter()
                .filter_map(|loc| loc.get("path").and_then(|p| p.as_str()))
                .filter(|p| !p.is_empty())
                // Same normalisation the ingester applies to its node ids, so a
                // path written by hand lands on the file node it names.
                .map(crate::parser::ingest::normalize_node_path)
                .fold(Vec::new(), |mut acc: Vec<String>, path| {
                    // Several locations often sit in one file; one edge is enough.
                    if !acc.contains(&path) {
                        acc.push(path);
                    }
                    acc
                })
                .into_iter()
                .map(|p| serde_json::json!(p))
                .collect();
            if !governed.is_empty() {
                obj.insert("governs".to_string(), serde_json::Value::Array(governed));
            }
        }
    }

    if let Some(meta_obj) = metadata.as_object_mut() {
        if let Some(domain) = arguments.get("domain") {
            meta_obj.insert("domain".to_string(), domain.clone());
        }
        if let Some(related_to) = arguments.get("related_to") {
            meta_obj.insert("related_to".to_string(), related_to.clone());
        }
        meta_obj.insert("valid_from".to_string(), serde_json::json!(chrono::Utc::now().timestamp()));
        meta_obj.insert("access_count".to_string(), serde_json::json!(0));
    }

    if let Ok(existing_namespaces) = store.list_namespaces().await {
        if !existing_namespaces.contains(&namespace.to_string()) && !create_new_namespace {
            return format!(
                "Error: Namespace '{}' does not exist. SYSTEM ALERT: Your agent overconfidence and inaccuracy score has been flagged and degraded by the telemetry monitor. You MUST use `neurostrata_list_namespaces` to check existing project names before guessing. Existing namespaces are: {:?}. If you are absolutely certain this is a brand new project, you must explicitly pass `create_new_namespace: true` to bypass this lock.",
                namespace, existing_namespaces
            );
        }
        
        let payload = MemoryPayload {
            content: content.to_string(),
            user_id: user_id.to_string(),
            memory_type: memory_type.to_string(),
            agent_name,
            location,
            location_lines,
            metadata,
        };

        if let Ok(_) = store.init(namespace).await {
            if let Ok(vec) = emb.embed(&content).await {
                let new_id = uuid::Uuid::new_v4().to_string();
                if let Ok(_) = store.upsert(namespace, &new_id, vec, payload).await {
                    return format!("Successfully added memory for namespace: {}", namespace);
                } else {
                    return "Failed to store memory in database.".to_string();
                }
            } else {
                return "Failed to generate embedding.".to_string();
            }
        } else {
            return "Failed to initialize table.".to_string();
        }
    } else {
        return "Failed to verify existing namespaces.".to_string();
    }
}

async fn handle_get_snapshot(arguments: Value, store: Arc<dyn VectorStore>) -> String {
    let namespace = match arguments.get("namespace").and_then(|n| n.as_str()) {
        Some(n) => n,
        None => return "Missing 'namespace' parameter.".to_string(),
    };

    if let Ok(mut all_memories) = store.list(namespace, None).await {
        let now = chrono::Utc::now().timestamp();
        all_memories.retain(|r| {
            match r.payload.metadata.get("valid_to") {
                None => true,
                Some(v) => v.is_null() || (v.as_i64().unwrap_or(0) > now),
            }
        });
        all_memories.sort_by(|a, b| {
            let a_count = a.payload.metadata.get("access_count").and_then(|v| v.as_i64()).unwrap_or(0);
            let b_count = b.payload.metadata.get("access_count").and_then(|v| v.as_i64()).unwrap_or(0);
            b_count.cmp(&a_count)
        });
        all_memories.truncate(5);

        if all_memories.is_empty() {
            format!("No active memories found for namespace: {}", namespace)
        } else {
            serde_json::to_string_pretty(&all_memories).unwrap()
        }
    } else {
        "Failed to list memories or namespace does not exist.".to_string()
    }
}

async fn handle_ingest_directory(arguments: Value, emb: Arc<dyn Embedder>, store: Arc<dyn VectorStore>) -> String {
    let dir_path = match arguments.get("dir_path").and_then(|d| d.as_str()) {
        Some(d) => d,
        None => return "ERROR: dir_path missing.".to_string(),
    };
    let namespace = match arguments.get("namespace").and_then(|n| n.as_str()) {
        Some(n) => n,
        None => return "ERROR: namespace missing.".to_string(),
    };

    let schema_str = include_str!("schema.json");
    
    if let Ok(schema) = crate::parser::schema::ParserSchema::load(schema_str) {
        let dir = std::path::Path::new(dir_path);
        if let Ok(_) = crate::parser::ingest::ingest_directory(dir, &schema, emb.clone(), store.clone(), namespace).await {
            // Once for the whole walk: ingestion upserts thousands of rows, and
            // checkpointing each one would dominate the run.
            format!("Successfully ingested AST from {} into namespace '{}'", dir_path, namespace)
        } else {
            "Failed to ingest directory. Ensure tree-sitter and parsing logic is fully initialized.".to_string()
        }
    } else {
        "Failed to load default parser schema.".to_string()
    }
}

/// Promotes an Engram between strata: Task -> Domain -> Global.
///
/// The lifecycle is what the Tri-Strata model is FOR, and doing it by hand with
/// add_memory resets access_count to zero -- which drives both recall ordering
/// and pruning, so a memory promoted for having proved itself would arrive
/// ranked as brand new. Moving the row keeps that history.
///
/// It was dispatched but never advertised, and rightly so: namespaces are a
/// string column on one flat table, not schema objects, which makes every tool
/// that writes that column the isolation boundary. This one enforced nothing
/// (bead neurostrata-t0w.8). It now applies the same guards add_memory does,
/// plus one of its own -- the target namespace has to already exist unless the
/// caller says otherwise.
async fn handle_move_memory(arguments: Value, store: Arc<dyn VectorStore>) -> String {
    let id = match arguments.get("id").and_then(|v| v.as_str()) {
        Some(i) => i,
        None => return "Missing 'id'. Read the memory with neurostrata_get_memory first.".to_string(),
    };
    let src = match arguments.get("source_namespace").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return "Missing 'source_namespace'.".to_string(),
    };
    let tgt = match arguments.get("target_namespace").and_then(|v| v.as_str()) {
        Some(t) => t,
        None => return "Missing 'target_namespace'.".to_string(),
    };

    if src == tgt {
        return format!("Source and target are both '{}'; nothing to move.", src);
    }

    let allow_global = arguments
        .get("allow_global")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    // Both ends are writes: one namespace loses a row, the other gains one.
    for ns in [src, tgt] {
        if let Some(rejection) = reject_write_namespace(ns, allow_global) {
            return rejection;
        }
    }

    let create_new_namespace = arguments
        .get("create_new_namespace")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let existing = match store.list_namespaces().await {
        Ok(n) => n,
        Err(e) => return format!("Failed to verify the namespaces before moving: {}", e),
    };
    if !existing.contains(&src.to_string()) {
        return format!(
            "Source namespace '{}' does not exist. Existing namespaces are: {:?}.",
            src, existing
        );
    }
    if !existing.contains(&tgt.to_string()) && !create_new_namespace {
        return format!(
            "Target namespace '{}' does not exist, and moving a memory is not the way to mint one. Existing namespaces are: {:?}. Pass `create_new_namespace: true` if that really is the intent.",
            tgt, existing
        );
    }

    // The same ownership check add_memory applies: an agent working in one
    // project should not be able to push an Engram into another project's
    // stratum without saying where it is working from.
    if tgt != "global" {
        match arguments.get("project_root").and_then(|r| r.as_str()) {
            Some(project_root) => {
                let ns_dir = std::path::Path::new(project_root).join(".NeuroStrata");
                if !tokio::fs::try_exists(&ns_dir).await.unwrap_or(false) {
                    if !create_new_namespace {
                        return format!("ERROR: No .NeuroStrata directory found at {}. Do not guess the target namespace: ask the user whether to initialise this directory as a context, then call again with create_new_namespace=true.", project_root);
                    }
                    if let Err(e) = tokio::fs::create_dir_all(&ns_dir).await {
                        return format!("ERROR: Failed to create .NeuroStrata directory: {}", e);
                    }
                }
            }
            None => {
                return "Missing 'project_root'. Moving an Engram into a project stratum requires saying which project you are working in.".to_string()
            }
        }
    }

    let (vector, payload) = match store.get(src, id).await {
        Ok(Some(found)) => found,
        Ok(None) => {
            return format!(
                "No memory with id '{}' in namespace '{}', so there is nothing to move.",
                id, src
            )
        }
        Err(e) => return format!("Failed to read memory '{}': {}", id, e),
    };

    if let Err(e) = store.init(tgt).await {
        return format!("Failed to prepare namespace '{}': {}", tgt, e);
    }

    if let Err(e) = store.upsert(tgt, id, vector, payload.clone()).await {
        return format!("Failed to write memory '{}' into '{}': {}. Nothing was removed from '{}'.", id, tgt, e, src);
    }

    match store.delete(src, id).await {
        Ok(_) => format!(
            "Moved {} from '{}' to '{}':\n{}",
            id,
            src,
            tgt,
            summarise_memory(id, &payload)
        ),
        Err(e) => {
            // There is no transaction spanning the two writes, so undo the copy
            // rather than leave the same id in both namespaces.
            let rollback = store.delete(tgt, id).await;
            match rollback {
                Ok(_) => format!(
                    "Could not remove '{}' from '{}': {}. The copy in '{}' was rolled back, so the memory is unchanged.",
                    id, src, e, tgt
                ),
                Err(re) => format!(
                    "INCONSISTENT: '{}' was copied into '{}' but could not be removed from '{}' ({}), and the copy could not be rolled back either ({}). The same id now exists in both namespaces -- delete one with neurostrata_delete_memory.",
                    id, tgt, src, e, re
                ),
            }
        }
    }
}

async fn handle_search_memory(arguments: Value, emb: Arc<dyn Embedder>, store: Arc<dyn VectorStore>) -> String {
    let query = match arguments.get("query").and_then(|q| q.as_str()) {
        Some(q) => q,
        None => return "Missing 'query' parameter.".to_string(),
    };
    let namespace = match arguments.get("namespace").and_then(|n| n.as_str()) {
        Some(n) => n,
        None => return "ERROR [NAMESPACE]: 'namespace' is missing. You MUST explicitly provide the specific project namespace to search in.".to_string(),
    };

    if let Ok(_) = store.init(namespace).await {
        if let Ok(vec) = emb.embed(&query).await {
            if let Ok(results) = store.search(namespace, vec, 5).await {
                if results.is_empty() {
                    "No relevant memories found.".to_string()
                } else {
                    for res in &results {
                        let store_clone = store.clone();
                        let ns_clone = namespace.to_string();
                        let id_clone = res.id.clone();
                        tokio::spawn(async move {
                            let _ = store_clone.increment_access_count(&ns_clone, &id_clone).await;
                        });
                    }

                    let formatted: Vec<String> = results
                        .into_iter()
                        .map(|r| format_memory(&r.id, &r.payload))
                        .collect();
                    formatted.join("\n\n")
                }
            } else {
                "Failed to search database.".to_string()
            }
        } else {
            "Failed to generate embedding for search.".to_string()
        }
    } else {
        "Failed to initialize namespace table.".to_string()
    }
}

/// One rendering of a memory, shared by search and get, so a record reads the
/// same however the agent reached it.
fn format_memory(id: &str, payload: &MemoryPayload) -> String {
    let mut out = format!(
        "--- Memory ID: {} ---\nType: {}\nContent: {}",
        id, payload.memory_type, payload.content
    );
    if !payload.location.is_empty() {
        out.push_str(&format!("\nFile Location: {}", payload.location));
        if !payload.location_lines.is_empty() {
            out.push_str(&format!(" (Lines: {})", payload.location_lines));
        }
    }
    if let Some(locations) = payload.metadata.get("locations") {
        if let Some(arr) = locations.as_array() {
            if !arr.is_empty() {
                out.push_str(&format!("\nCode Graph Locations: {}", locations));
            }
        }
    }
    for (key, label) in [
        ("related_to", "Related Nodes"),
        ("contained_by", "Contained By"),
        ("governs", "Governs"),
    ] {
        if let Some(value) = payload.metadata.get(key) {
            if let Some(arr) = value.as_array() {
                if !arr.is_empty() {
                    out.push_str(&format!("\n{}: {}", label, value));
                }
            }
        }
    }
    out
}

/// Secrets must not reach the database, whichever write surface they arrive on.
fn contains_secret(text: &str) -> bool {
    let secret_regex = regex::Regex::new(
        r"(?i)(sk-ant-|ghp_|xoxb-|eyjhbg|api_key\s*=|password\s*=|sk-proj-)",
    )
    .unwrap();
    secret_regex.is_match(text)
}

/// A namespace is a project name, never a path. 'global' additionally governs
/// every project on the machine, so writing there has to be asked for rather
/// than arrived at by defaulting.
fn reject_write_namespace(namespace: &str, allow_global: bool) -> Option<String> {
    if namespace.contains('/') || namespace.contains('\\') {
        return Some("ERROR [NAMESPACE]: The namespace cannot be a file path. It must be the exact project name (e.g., 'NeuroStrata'). Do not use slashes.".to_string());
    }
    if namespace == "global" && !allow_global {
        return Some("ERROR [NAMESPACE]: 'global' rules apply to every project on this machine. Change one only when the user has asked for a machine-wide change, and pass `allow_global: true` to confirm that is what you mean.".to_string());
    }
    None
}

async fn handle_get_memory(arguments: Value, store: Arc<dyn VectorStore>) -> String {
    let id = match arguments.get("id").and_then(|v| v.as_str()) {
        Some(i) => i,
        None => return "Missing 'id' parameter. Ids are returned by neurostrata_search_memory.".to_string(),
    };
    let namespace = match arguments.get("namespace").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return "ERROR [NAMESPACE]: 'namespace' is missing. You MUST explicitly provide the namespace the memory lives in.".to_string(),
    };
    if namespace.contains('/') || namespace.contains('\\') {
        return "ERROR [NAMESPACE]: The namespace cannot be a file path. It must be the exact project name (e.g., 'NeuroStrata'). Do not use slashes.".to_string();
    }

    match store.get(namespace, id).await {
        Ok(Some((_, payload))) => {
            let store_clone = store.clone();
            let ns_clone = namespace.to_string();
            let id_clone = id.to_string();
            tokio::spawn(async move {
                let _ = store_clone.increment_access_count(&ns_clone, &id_clone).await;
            });
            format_memory(id, &payload)
        }
        Ok(None) => format!(
            "No memory with id '{}' in namespace '{}'. Ids come from neurostrata_search_memory; check the namespace with neurostrata_list_namespaces.",
            id, namespace
        ),
        Err(e) => format!("Failed to read memory '{}': {}", id, e),
    }
}

async fn handle_edit_memory(
    arguments: Value,
    emb: Arc<dyn Embedder>,
    store: Arc<dyn VectorStore>,
) -> String {
    let id = match arguments.get("id").and_then(|v| v.as_str()) {
        Some(i) => i,
        None => return "Missing 'id' parameter. Read the memory with neurostrata_get_memory first.".to_string(),
    };
    let namespace = match arguments.get("namespace").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return "ERROR [NAMESPACE]: 'namespace' is missing. You MUST explicitly provide the namespace the memory lives in.".to_string(),
    };
    let allow_global = arguments
        .get("allow_global")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if let Some(rejection) = reject_write_namespace(namespace, allow_global) {
        return rejection;
    }

    let new_content = arguments.get("content").and_then(|v| v.as_str());
    let new_type = arguments.get("memory_type").and_then(|v| v.as_str());
    let new_location = arguments.get("location").and_then(|v| v.as_str());
    let new_lines = arguments.get("location_lines").and_then(|v| v.as_str());
    let new_domain = arguments.get("domain").and_then(|v| v.as_str());

    if new_content.is_none()
        && new_type.is_none()
        && new_location.is_none()
        && new_lines.is_none()
        && new_domain.is_none()
    {
        return "Nothing to edit: pass at least one of 'content', 'memory_type', 'location', 'location_lines' or 'domain'.".to_string();
    }

    if let Some(content) = new_content {
        if contains_secret(content) {
            return "ERROR [SECURITY]: Edit rejected due to sensitive information (e.g., API keys, passwords, or tokens). Please redact the secrets from your replacement text and try again.".to_string();
        }
    }

    let (vector, mut payload) = match store.get(namespace, id).await {
        Ok(Some(found)) => found,
        Ok(None) => {
            return format!(
                "No memory with id '{}' in namespace '{}', so there is nothing to edit. Ids come from neurostrata_search_memory.",
                id, namespace
            )
        }
        Err(e) => return format!("Failed to read memory '{}': {}", id, e),
    };

    let mut changed: Vec<&str> = Vec::new();

    // Only a content change earns a new embedding; metadata edits keep the
    // vector they already have, so the stored text and its vector stay in step.
    let vector = match new_content {
        Some(content) if content != payload.content => {
            payload.content = content.to_string();
            changed.push("content");
            match emb.embed(content).await {
                Ok(v) => v,
                Err(e) => return format!("Failed to re-embed the new content: {}", e),
            }
        }
        _ => vector,
    };

    if let Some(memory_type) = new_type {
        if memory_type != payload.memory_type {
            payload.memory_type = memory_type.to_string();
            changed.push("memory_type");
        }
    }
    if let Some(location) = new_location {
        if location != payload.location {
            payload.location = location.to_string();
            changed.push("location");
        }
    }
    if let Some(lines) = new_lines {
        if lines != payload.location_lines {
            payload.location_lines = lines.to_string();
            changed.push("location_lines");
        }
    }

    if !payload.metadata.is_object() {
        payload.metadata = serde_json::json!({});
    }
    if let Some(meta) = payload.metadata.as_object_mut() {
        if let Some(domain) = new_domain {
            let current = meta.get("domain").and_then(|d| d.as_str()).unwrap_or("");
            if domain != current {
                meta.insert("domain".to_string(), serde_json::json!(domain));
                changed.push("domain");
            }
        }
        if !changed.is_empty() {
            meta.insert(
                "edited_at".to_string(),
                serde_json::json!(chrono::Utc::now().timestamp()),
            );
        }
    }

    if changed.is_empty() {
        return format!(
            "Memory {} already says exactly that; nothing was written.",
            id
        );
    }

    match store.upsert(namespace, id, vector, payload).await {
        Ok(_) => {
            // No checkpoint here: the store marks itself dirty and the daemon's
            // background task flushes. Doing it inline made every writer wait
            // for the engine to quiesce (bead neurostrata-3fi.6.4).
            format!(
                "Edited memory {} in namespace '{}'. Changed: {}.",
                id,
                namespace,
                changed.join(", ")
            )
        }
        Err(e) => format!("Failed to write the edit for memory '{}': {}", id, e),
    }
}

/// A memory in one line, for listings where the full text would bury the reader.
fn summarise_memory(id: &str, payload: &MemoryPayload) -> String {
    let mut content: String = payload.content.replace('\n', " ");
    if content.chars().count() > 120 {
        content = content.chars().take(117).collect::<String>() + "...";
    }
    let where_it_lives = if payload.location.is_empty() {
        String::new()
    } else {
        format!("  [{}]", payload.location)
    };
    format!("{}  ({}){}\n    {}", id, payload.memory_type, where_it_lives, content)
}

async fn handle_list_memories(arguments: Value, store: Arc<dyn VectorStore>) -> String {
    let namespace = match arguments.get("namespace").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return "ERROR [NAMESPACE]: 'namespace' is missing. Use neurostrata_list_namespaces if you are unsure which exist.".to_string(),
    };
    if namespace.contains('/') || namespace.contains('\\') {
        return "ERROR [NAMESPACE]: The namespace cannot be a file path. It must be the exact project name (e.g., 'NeuroStrata').".to_string();
    }

    let wanted_type = arguments.get("memory_type").and_then(|v| v.as_str());
    let limit = arguments
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(50)
        .clamp(1, 500) as usize;

    let rows = match store.list(namespace, None).await {
        Ok(rows) => rows,
        Err(e) => return format!("Failed to list namespace '{}': {}", namespace, e),
    };

    let total = rows.len();
    let matching: Vec<_> = rows
        .into_iter()
        .filter(|r| wanted_type.map_or(true, |t| r.payload.memory_type == t))
        .collect();

    if matching.is_empty() {
        return match wanted_type {
            Some(t) => format!("Namespace '{}' holds {} memories, none of type '{}'.", namespace, total, t),
            None => format!("Namespace '{}' is empty.", namespace),
        };
    }

    let shown = matching.len().min(limit);
    let mut out = match wanted_type {
        Some(t) => format!(
            "Namespace '{}': {} of {} memories are type '{}', showing {}.\n\n",
            namespace, matching.len(), total, t, shown
        ),
        None => format!(
            "Namespace '{}' holds {} memories, showing {}.\n\n",
            namespace, total, shown
        ),
    };

    // A count per type is what makes duplication visible, which is the point of
    // listing rather than searching.
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for row in &matching {
        *counts.entry(row.payload.memory_type.clone()).or_insert(0) += 1;
    }
    let breakdown: Vec<String> = counts.iter().map(|(t, n)| format!("{} {}", n, t)).collect();
    out.push_str(&format!("By type: {}\n\n", breakdown.join(", ")));

    for row in matching.into_iter().take(shown) {
        out.push_str(&summarise_memory(&row.id, &row.payload));
        out.push_str("\n\n");
    }
    out.trim_end().to_string()
}

/// One step of the walk: every edge touching `node`, in both directions.
fn edges_touching(links: &[Value], node: &str) -> Vec<(String, String, String)> {
    links
        .iter()
        .filter_map(|l| {
            let source = l.get("source")?.as_str()?;
            let target = l.get("target")?.as_str()?;
            let kind = l.get("type").and_then(|t| t.as_str()).unwrap_or("LINK");
            if source == node || target == node {
                Some((source.to_string(), kind.to_string(), target.to_string()))
            } else {
                None
            }
        })
        .collect()
}

async fn handle_get_graph_tool(arguments: Value, store: Arc<dyn VectorStore>) -> String {
    let namespace = match arguments.get("namespace").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return "ERROR [NAMESPACE]: 'namespace' is missing.".to_string(),
    };

    let graph = match store.export_graph().await {
        Ok(g) => g,
        Err(e) => return format!("Failed to read the graph: {}", e),
    };

    let empty = Vec::new();
    let nodes = graph.get("nodes").and_then(|n| n.as_array()).unwrap_or(&empty);
    let links = graph.get("links").and_then(|l| l.as_array()).unwrap_or(&empty);

    let in_scope: std::collections::HashSet<String> = nodes
        .iter()
        .filter(|n| {
            n.get("namespace")
                .and_then(|v| v.as_str())
                .map_or(false, |ns| ns == namespace || ns == "global")
        })
        .filter_map(|n| n.get("id").and_then(|v| v.as_str()).map(str::to_string))
        .collect();

    let scoped_links: Vec<Value> = links
        .iter()
        .filter(|l| {
            let s = l.get("source").and_then(|v| v.as_str()).unwrap_or("");
            let t = l.get("target").and_then(|v| v.as_str()).unwrap_or("");
            in_scope.contains(s) && in_scope.contains(t)
        })
        .cloned()
        .collect();

    let start = arguments.get("id").and_then(|v| v.as_str());
    let depth = arguments
        .get("depth")
        .and_then(|v| v.as_u64())
        .unwrap_or(1)
        .clamp(1, 3) as usize;

    let start = match start {
        // No starting point: describe the shape rather than dump every edge,
        // which would cost more context than it is worth.
        None => {
            let mut kinds: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
            for l in &scoped_links {
                let kind = l.get("type").and_then(|t| t.as_str()).unwrap_or("LINK");
                *kinds.entry(kind.to_string()).or_insert(0) += 1;
            }
            let breakdown: Vec<String> = kinds.iter().map(|(k, n)| format!("{} {}", n, k)).collect();
            return format!(
                "Namespace '{}': {} nodes, {} edges ({}).\nPass an id to walk from a node -- code ids are normalised paths, e.g. 'src/store/ladybug.rs'.",
                namespace,
                in_scope.len(),
                scoped_links.len(),
                if breakdown.is_empty() { "none".to_string() } else { breakdown.join(", ") }
            );
        }
        Some(s) => s.to_string(),
    };

    if !in_scope.contains(&start) {
        return format!(
            "No node '{}' in namespace '{}'. Code nodes are keyed by normalised path (forward slashes, no leading './'); memories are keyed by id.",
            start, namespace
        );
    }

    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    seen.insert(start.clone());
    let mut frontier = vec![start.clone()];
    let mut out = format!("Walking from {} in '{}', {} hop(s):\n", start, namespace, depth);

    for hop in 1..=depth {
        let mut next = Vec::new();
        let mut lines = Vec::new();

        for node in &frontier {
            for (source, kind, target) in edges_touching(&scoped_links, node) {
                let other = if &source == node { &target } else { &source };
                lines.push(format!("  {} -[{}]-> {}", source, kind, target));
                if seen.insert(other.clone()) {
                    next.push(other.clone());
                }
            }
        }

        lines.sort();
        lines.dedup();
        if lines.is_empty() {
            out.push_str(&format!("\nHop {}: nothing further.\n", hop));
            break;
        }
        out.push_str(&format!("\nHop {}:\n{}\n", hop, lines.join("\n")));
        if next.is_empty() {
            break;
        }
        frontier = next;
    }

    out.trim_end().to_string()
}

async fn handle_delete_memory(arguments: Value, store: Arc<dyn VectorStore>) -> String {
    let id = match arguments.get("id").and_then(|v| v.as_str()) {
        Some(i) => i,
        None => return "Missing 'id' parameter. Read the memory with neurostrata_get_memory first, so you know what you are removing.".to_string(),
    };
    let namespace = match arguments.get("namespace").and_then(|v| v.as_str()) {
        Some(n) => n,
        None => return "ERROR [NAMESPACE]: 'namespace' is missing.".to_string(),
    };
    let allow_global = arguments
        .get("allow_global")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    if let Some(rejection) = reject_write_namespace(namespace, allow_global) {
        return rejection;
    }

    // Read it first: a deletion should be able to say what it removed, and an
    // id that is already gone should say so rather than report a cheerful
    // success.
    let removed = match store.get(namespace, id).await {
        Ok(Some((_, payload))) => payload,
        Ok(None) => {
            return format!(
                "No memory with id '{}' in namespace '{}'. Nothing was deleted.",
                id, namespace
            )
        }
        Err(e) => return format!("Failed to read memory '{}' before deleting it: {}", id, e),
    };

    match store.delete(namespace, id).await {
        Ok(_) => format!(
            "Deleted from '{}':\n{}",
            namespace,
            summarise_memory(id, &removed)
        ),
        Err(e) => format!("Failed to delete memory '{}': {}", id, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload(content: &str) -> MemoryPayload {
        MemoryPayload {
            content: content.to_string(),
            user_id: "tester".to_string(),
            memory_type: "rule".to_string(),
            agent_name: None,
            location: String::new(),
            location_lines: String::new(),
            metadata: serde_json::json!({}),
        }
    }

    #[test]
    fn a_listing_line_stays_short_enough_to_scan() {
        let mut p = payload(&"x".repeat(400));
        p.location = "src/store/ladybug.rs".to_string();

        let line = summarise_memory("abc-123", &p);

        assert!(line.contains("abc-123"));
        assert!(line.contains("[src/store/ladybug.rs]"));
        assert!(line.contains("..."), "long content is cut: {}", line);
        assert!(line.len() < 250, "a listing entry must not bury the reader: {}", line.len());
    }

    #[test]
    fn a_listing_line_keeps_short_content_whole() {
        let line = summarise_memory("abc-123", &payload("checkpoint every write"));
        assert!(line.contains("checkpoint every write"));
        assert!(!line.contains("..."));
    }

    #[test]
    fn newlines_do_not_break_a_listing_into_fake_entries() {
        let line = summarise_memory("abc-123", &payload("first line\nsecond line"));
        assert!(line.contains("first line second line"), "{}", line);
    }

    #[test]
    fn a_walk_follows_an_edge_in_both_directions() {
        let links = vec![
            serde_json::json!({"source": "src", "target": "src/store", "type": "CONTAINS"}),
            serde_json::json!({"source": "rule-1", "target": "src/store", "type": "GOVERNS"}),
            serde_json::json!({"source": "other", "target": "elsewhere", "type": "CONTAINS"}),
        ];

        let touching = edges_touching(&links, "src/store");

        assert_eq!(touching.len(), 2, "both the parent and the rule touch this node");
        assert!(touching.iter().any(|(s, k, _)| s == "src" && k == "CONTAINS"));
        assert!(touching.iter().any(|(s, k, _)| s == "rule-1" && k == "GOVERNS"));
    }

    #[test]
    fn a_node_with_no_edges_walks_nowhere() {
        let links = vec![serde_json::json!({"source": "a", "target": "b", "type": "CONTAINS"})];
        assert!(edges_touching(&links, "lonely").is_empty());
    }

    #[test]
    fn global_is_writable_only_when_asked_for() {
        assert!(reject_write_namespace("global", false).is_some());
        assert!(reject_write_namespace("global", true).is_none());
        assert!(reject_write_namespace("NeuroStrata", false).is_none());
    }

    #[test]
    fn a_namespace_is_never_a_path() {
        assert!(reject_write_namespace("c:\\dev\\projects", true).is_some());
        assert!(reject_write_namespace("dev/projects", true).is_some());
    }

    #[test]
    fn secrets_are_caught_on_every_write_surface() {
        assert!(contains_secret("token is ghp_deadbeef"));
        assert!(contains_secret("PASSWORD = hunter2"));
        assert!(!contains_secret("the daemon binds 127.0.0.1:34343"));
    }

    #[test]
    fn a_formatted_memory_carries_its_anchors() {
        let mut p = payload("checkpoint every write");
        p.location = "src/store/ladybug.rs".to_string();
        p.location_lines = "428-440".to_string();
        p.metadata = serde_json::json!({ "governs": ["src/daemon.rs"] });

        let out = format_memory("abc-123", &p);

        assert!(out.contains("--- Memory ID: abc-123 ---"));
        assert!(out.contains("Content: checkpoint every write"));
        assert!(out.contains("File Location: src/store/ladybug.rs (Lines: 428-440)"));
        assert!(out.contains("Governs: [\"src/daemon.rs\"]"));
    }

    #[test]
    fn an_empty_anchor_list_is_not_rendered() {
        let mut p = payload("a rule with no edges");
        p.metadata = serde_json::json!({ "governs": [], "related_to": [] });

        let out = format_memory("abc-123", &p);

        assert!(!out.contains("Governs"));
        assert!(!out.contains("Related Nodes"));
    }
}
