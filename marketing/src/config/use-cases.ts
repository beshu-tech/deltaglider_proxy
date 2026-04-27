import type { LucideIcon } from 'lucide-react';
import {
  BookOpen,
  CloudCog,
  Github,
  LayoutGrid,
  Network,
  Package,
  ServerCog,
  Shield,
} from 'lucide-react';

/** Kept in sync with deployment cards and marketing accents. */
export type DeploymentPathAccent = 'ember' | 'violet' | 'cyan' | 'sky' | 'emerald';

/**
 * One row per use-case URL: shared by mega menu, mobile nav, deployment grid, carousel, footer.
 */
export interface UseCasePath {
  /** Route (trailing slash) */
  to: string;
  /** Short label in nav / footer */
  navLabel: string;
  /** One line in the mega menu */
  summary: string;
  /** Section “voice” line on deployment cards */
  voice: string;
  /** Card title on / */
  who: string;
  /** Card body on / */
  payoff: string;
  accent: DeploymentPathAccent;
  /** Lucide icon (24px default from library) */
  icon: LucideIcon;
}

export const USE_CASE_PATHS: readonly UseCasePath[] = [
  {
    to: '/regulated/',
    navLabel: 'Regulated workloads',
    summary:
      'Use cheaper object storage without handing it plaintext or key custody.',
    voice: 'Security & compliance',
    who: 'Regulated workloads',
    payoff:
      'Use cheap or untrusted storage safely: encrypt before the backend, keep keys on trusted premises, then add compression.',
    accent: 'ember',
    icon: Shield,
  },
  {
    to: '/artifact-storage/',
    navLabel: 'Artifact storage',
    summary:
      'Compress repeated builds, backups, dumps, models, and package catalogs.',
    voice: 'Storage efficiency',
    who: 'Artifact storage',
    payoff:
      'Store backup archives, software catalogs, media asset variants, and AI model variants as deltas.',
    accent: 'violet',
    icon: Package,
  },
  {
    to: '/s3-to-hetzner-wasabi/',
    navLabel: 'AWS migration',
    summary: 'Move hot S3 API workloads to lower-cost backends and keep the controls.',
    voice: 'Migration economics',
    who: 'S3 to Hetzner / Wasabi',
    payoff:
      'Model storage-price reduction and compression while keeping enterprise S3 controls in DeltaGlider.',
    accent: 'cyan',
    icon: CloudCog,
  },
  {
    to: '/multi-cloud-control-plane/',
    navLabel: 'Multi-cloud control plane',
    summary:
      'One policy, identity, encryption, audit, and replication layer over many stores.',
    voice: 'Multi-cloud control',
    who: 'One S3 security layer',
    payoff:
      'Unify aliases, IAM, encryption, audit, and replication across on-prem, Hetzner, Wasabi, or another backend.',
    accent: 'sky',
    icon: Network,
  },
  {
    to: '/minio-migration/',
    navLabel: 'MinIO migration',
    summary: 'Keep self-hosted S3 and bring back IAM, OAuth, quotas, replication, and UI.',
    voice: 'Enterprise control plane',
    who: 'MinIO migration',
    payoff:
      'Keep an open-source S3 engine for bytes; add the enterprise controls you still need: IAM, OAuth, policy, quotas, replication, and admin UI.',
    accent: 'emerald',
    icon: ServerCog,
  },
] as const;

const byPath = new Map(USE_CASE_PATHS.map((p) => [p.to, p] as const));

export function getUseCaseByPath(path: string): UseCasePath | undefined {
  return byPath.get(path);
}

/** Re-export for pages that need type parity with the grid (alias). */
export type DeploymentPath = UseCasePath;

/** Top-level and mega-menu: consistent Lucide building blocks. */
export const siteNavIcon = {
  useCases: LayoutGrid,
  docs: BookOpen,
  github: Github,
} as const;
