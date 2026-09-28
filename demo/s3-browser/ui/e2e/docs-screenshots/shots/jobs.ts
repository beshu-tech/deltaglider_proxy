/**
 * Jobs shots: replication and lifecycle rules and their runs (docs:
 * how-to/replicate-a-bucket, how-to/expire-and-archive-objects,
 * explanation/jobs-and-durability, reference/jobs).
 */
import type { Shot } from '../shot';
import { nav } from './common';

export const JOBS_SHOTS: Shot[] = [
  {
    id: 'jobs-list',
    route: '/_/admin/jobs',
    alt: 'The Jobs page lists the releases-to-dr replication rule and the expire-old-downloads lifecycle rule in one table; callout 1 marks Jobs in the sidebar and callout 2 marks New job.',
    annotations: [
      { target: nav('Jobs'), kind: 'callout', label: '1', side: 'right' },
      { target: { role: 'button', name: 'New job' }, kind: 'box', label: '2' },
    ],
  },
  {
    id: 'replication-rule-editor',
    route: '/_/admin/jobs?job=replication:releases-to-dr&tab=definition',
    alt: 'The definition of the releases-to-dr replication rule copies the releases bucket to the releases-dr bucket; arrow 1 marks the source bucket and arrow 2 marks the destination bucket.',
    annotations: [
      { target: { css: 'input[value="releases"]' }, kind: 'arrow', side: 'top', label: '1' },
      { target: { css: 'input[value="releases-dr"]' }, kind: 'arrow', side: 'top', label: '2' },
    ],
  },
  {
    id: 'job-runs',
    route: '/_/admin/jobs?job=replication:releases-to-dr&tab=runs',
    alt: 'The Runs tab of the replication job shows one finished run that copied five objects; the arrow points at the number of copied objects.',
    annotations: [{ target: { text: /5 copied/ }, kind: 'arrow', side: 'right' }],
  },
  {
    id: 'lifecycle-rule-editor',
    route: '/_/admin/jobs?job=lifecycle:expire-old-downloads&tab=definition',
    alt: 'The definition of the expire-old-downloads lifecycle rule deletes objects in the downloads bucket that are older than 30 days; the box marks Expire after.',
    annotations: [{ target: { union: [{ text: 'Expire after', exact: true }, { css: 'input[value="30d"]' }, { text: /^Objects whose created_at/ }] }, kind: 'box' }],
  },
];
