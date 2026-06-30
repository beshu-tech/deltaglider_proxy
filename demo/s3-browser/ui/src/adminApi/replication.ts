// === Object Replication ===

type ReplicationConflictPolicy = 'newer-wins' | 'content-diff' | 'skip-if-dest-exists';

interface ReplicationEndpoint {
  bucket: string;
  prefix: string;
}

export interface ReplicationRuleConfig {
  name: string;
  enabled: boolean;
  source: ReplicationEndpoint;
  destination: ReplicationEndpoint;
  interval: string;
  batch_size: number;
  replicate_deletes: boolean;
  conflict: ReplicationConflictPolicy;
  include_globs: string[];
  exclude_globs: string[];
}

export interface ReplicationConfig {
  enabled: boolean;
  tick_interval: string;
  lease_ttl: string;
  heartbeat_interval: string;
  max_failures_retained: number;
  rules: ReplicationRuleConfig[];
}
