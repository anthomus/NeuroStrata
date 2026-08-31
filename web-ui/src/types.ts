export interface MemoryNode {
  id: string;
  name: string;
  memory_type: string;
  namespace?: string;
  agent_name?: string;
  degree: number;
  [key: string]: any;
}

export interface MemoryLink {
  source: string;
  target: string;
  type?: string;
  [key: string]: any;
}

export interface GraphData {
  nodes: MemoryNode[];
  links: MemoryLink[];
}

/// What the daemon reports for a walk started without waiting for it.
/// Mirrors IngestProgress in src/ingest_jobs.rs; the optional fields are the
/// ones it omits until they have a value.
export interface IngestProgress {
  namespace: string;
  dir: string;
  state: 'running' | 'finished' | 'failed';
  error?: string;
  /// Every file the walk has finished with, including those with no symbols.
  files_seen: number;
  /// The subset that produced at least one symbol.
  files_ingested: number;
  symbols_ingested: number;
  last_file?: string;
  relinked_edges?: number;
  started_unix: number;
  finished_unix?: number;
}
