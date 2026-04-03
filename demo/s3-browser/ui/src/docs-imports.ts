// Central registry of all docs — imported as raw strings via Vite
// Grouped for sidebar navigation

import README from '../../../../docs/README.md?raw';
import OPERATIONS from '../../../../docs/OPERATIONS.md?raw';
import CONFIGURATION from '../../../../docs/CONFIGURATION.md?raw';
import AUTHENTICATION from '../../../../docs/AUTHENTICATION.md?raw';
import HOWTO_SECURITY_BASICS from '../../../../docs/HOWTO_SECURITY_BASICS.md?raw';
import HOWTO_IAM_CONDITIONS from '../../../../docs/HOWTO_IAM_CONDITIONS.md?raw';
import RATE_LIMITING from '../../../../docs/RATE_LIMITING.md?raw';
import DELTA_RECONSTRUCTION from '../../../../docs/DELTA_RECONSTRUCTION.md?raw';
import STORAGE_FORMAT from '../../../../docs/STORAGE_FORMAT.md?raw';
import METRICS from '../../../../docs/METRICS.md?raw';
import CONTRIBUTING from '../../../../docs/CONTRIBUTING.md?raw';
import RELEASING from '../../../../docs/RELEASING.md?raw';

export interface DocEntry {
  id: string;
  title: string;
  filename: string;
  content: string;
  group: string;
}

export const DOC_GROUPS = [
  'Getting Started',
  'Security',
  'Internals',
  'Operations',
] as const;

export const DOCS: DocEntry[] = [
  // Getting Started
  { id: 'readme', title: 'Overview', filename: 'README.md', content: README, group: 'Getting Started' },
  { id: 'operations', title: 'Operations', filename: 'OPERATIONS.md', content: OPERATIONS, group: 'Getting Started' },
  { id: 'configuration', title: 'Configuration', filename: 'CONFIGURATION.md', content: CONFIGURATION, group: 'Getting Started' },

  // Security
  { id: 'authentication', title: 'Authentication', filename: 'AUTHENTICATION.md', content: AUTHENTICATION, group: 'Security' },
  { id: 'security-basics', title: 'Security Basics', filename: 'HOWTO_SECURITY_BASICS.md', content: HOWTO_SECURITY_BASICS, group: 'Security' },
  { id: 'iam-conditions', title: 'IAM Conditions', filename: 'HOWTO_IAM_CONDITIONS.md', content: HOWTO_IAM_CONDITIONS, group: 'Security' },
  { id: 'rate-limiting', title: 'Rate Limiting', filename: 'RATE_LIMITING.md', content: RATE_LIMITING, group: 'Security' },

  // Internals
  { id: 'delta-reconstruction', title: 'Delta Reconstruction', filename: 'DELTA_RECONSTRUCTION.md', content: DELTA_RECONSTRUCTION, group: 'Internals' },
  { id: 'storage-format', title: 'Storage Format', filename: 'STORAGE_FORMAT.md', content: STORAGE_FORMAT, group: 'Internals' },
  { id: 'metrics', title: 'Metrics', filename: 'METRICS.md', content: METRICS, group: 'Internals' },

  // Operations
  { id: 'contributing', title: 'Contributing', filename: 'CONTRIBUTING.md', content: CONTRIBUTING, group: 'Operations' },
  { id: 'releasing', title: 'Releasing', filename: 'RELEASING.md', content: RELEASING, group: 'Operations' },
];

/** Find a doc by its .md filename (for inter-page link resolution) */
export function findDocByFilename(filename: string): DocEntry | undefined {
  return DOCS.find(d => d.filename === filename || d.filename === filename.replace(/^.*\//, ''));
}
