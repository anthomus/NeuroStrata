export interface MemoryNode {
  id: string;
  name: string;
  memory_type: string;
  namespace?: string;
  agent_name?: string;
  degree: number;
  content?: string;
  location?: string;
  absolute_path?: string;
  /** d3-force adds positional state at runtime */
  x?: number;
  y?: number;
  z?: number;
  vx?: number;
  vy?: number;
  vz?: number;
  [key: string]: unknown;
}

export interface MemoryLink {
  type?: string;
  [key: string]: unknown;
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
  files_seen: number;
  files_ingested: number;
  symbols_ingested: number;
  last_file?: string;
  relinked_edges?: number;
  started_unix: number;
  finished_unix?: number;
}
