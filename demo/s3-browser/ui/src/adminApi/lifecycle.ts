// === Object Lifecycle ===
import type { AdminConfig } from './core';
import type { ReplicationConfig } from './replication';

export interface LifecycleQualifySpec {
  min_size_bytes?: number;
  min_age?: string;
}

export type LifecycleAction =
  | 'delete'
  | {
      type: 'transition' | 'archive';
      destination: {
        bucket: string;
        prefix?: string;
      };
      delete_source_after_success?: boolean;
    }
  | {
      type: 'retain-newest';
      count: number;
      qualify?: LifecycleQualifySpec;
      protect_younger_than?: string;
    };

export interface LifecycleRuleConfig {
  name: string;
  enabled: boolean;
  bucket: string;
  prefix: string;
  action?: LifecycleAction;
  /** Required for delete/transition; omitted for retain-newest (count-based). */
  expire_after?: string;
  include_globs: string[];
  exclude_globs: string[];
  batch_size: number;
}

export interface LifecycleConfig {
  enabled: boolean;
  tick_interval: string;
  max_failures_retained: number;
  rules: LifecycleRuleConfig[];
}

export interface StorageSectionBody {
  buckets?: AdminConfig['bucket_policies'];
  replication?: ReplicationConfig;
  lifecycle?: LifecycleConfig;
}




