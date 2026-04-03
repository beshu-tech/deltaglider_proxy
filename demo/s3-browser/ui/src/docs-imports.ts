// Central registry of all docs — imported as raw strings via Vite
// Titles are extracted from the first `# heading` in each file (single source of truth)

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
import CI_INFRA from '../../../../docs/CI_INFRA.md?raw';
import HARDENING_PLAN from '../../../../docs/HARDENING_PLAN.md?raw';

/** Extract the first `# heading` from markdown content */
function extractTitle(content: string): string {
  for (const line of content.split('\n')) {
    const m = line.match(/^#\s+(.+)/);
    if (m) return m[1].trim();
  }
  return 'Untitled';
}

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

// Only filename, content, and group are manually configured.
// Title is derived from the first # heading in each markdown file.
const RAW_DOCS: { filename: string; content: string; group: string }[] = [
  // Getting Started
  { filename: 'README.md', content: README, group: 'Getting Started' },
  { filename: 'OPERATIONS.md', content: OPERATIONS, group: 'Getting Started' },
  { filename: 'CONFIGURATION.md', content: CONFIGURATION, group: 'Getting Started' },

  // Security
  { filename: 'AUTHENTICATION.md', content: AUTHENTICATION, group: 'Security' },
  { filename: 'HOWTO_SECURITY_BASICS.md', content: HOWTO_SECURITY_BASICS, group: 'Security' },
  { filename: 'HOWTO_IAM_CONDITIONS.md', content: HOWTO_IAM_CONDITIONS, group: 'Security' },
  { filename: 'RATE_LIMITING.md', content: RATE_LIMITING, group: 'Security' },
  { filename: 'HARDENING_PLAN.md', content: HARDENING_PLAN, group: 'Security' },

  // Internals
  { filename: 'DELTA_RECONSTRUCTION.md', content: DELTA_RECONSTRUCTION, group: 'Internals' },
  { filename: 'STORAGE_FORMAT.md', content: STORAGE_FORMAT, group: 'Internals' },
  { filename: 'METRICS.md', content: METRICS, group: 'Internals' },

  // Operations
  { filename: 'CONTRIBUTING.md', content: CONTRIBUTING, group: 'Operations' },
  { filename: 'RELEASING.md', content: RELEASING, group: 'Operations' },
  { filename: 'CI_INFRA.md', content: CI_INFRA, group: 'Operations' },
];

export const DOCS: DocEntry[] = RAW_DOCS.map(d => ({
  id: d.filename.replace(/\.md$/, '').toLowerCase().replace(/[^a-z0-9]+/g, '-'),
  title: extractTitle(d.content),
  filename: d.filename,
  content: d.content,
  group: d.group,
}));

/** Find a doc by its .md filename (for inter-page link resolution) */
export function findDocByFilename(filename: string): DocEntry | undefined {
  const bare = filename.replace(/^.*\//, '');
  return DOCS.find(d => d.filename === bare);
}
