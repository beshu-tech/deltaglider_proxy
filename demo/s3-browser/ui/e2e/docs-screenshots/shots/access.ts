/**
 * Access shots: users, groups, request rules, sign-in (docs: how-to/create-iam-users,
 * how-to/restrict-access-with-conditions, how-to/gate-requests-with-admission-rules,
 * tutorials/secure-your-proxy).
 */
import type { Page } from '@playwright/test';
import type { Shot } from '../shot';

/** Select a row of a master-detail list (users, groups) by its name. */
async function pick(page: Page, name: string): Promise<void> {
  await page.getByText(name, { exact: true }).first().click();
}

/** The user and group editors are taller than one screen. */
const TALL = { width: 1280, height: 1400 };

export const ACCESS_SHOTS: Shot[] = [
  {
    id: 'users-ci-uploader',
    route: '/_/admin/access/users',
    alt: 'The Users page shows the ci-uploader user; the box marks its one rule, which lets it read, write and list only under releases/firmware/.',
    setup: (page) => pick(page, 'ci-uploader'),
    viewport: TALL,
    annotations: [{ target: { text: 'releases/firmware/*' }, kind: 'box' }],
  },
  {
    id: 'user-ip-condition',
    route: '/_/admin/access/users',
    alt: 'The rule of the backup-bot user applies only to requests from the 203.0.113.0/24 network; the box marks that condition.',
    setup: (page) => pick(page, 'backup-bot'),
    viewport: TALL,
    annotations: [{ target: { label: /IP restriction/ }, kind: 'box' }],
  },
  {
    id: 'group-engineering',
    route: '/_/admin/access/groups',
    alt: 'The Engineering group has two members, dana and backup-bot, and one shared rule; the box marks the members.',
    setup: (page) => pick(page, 'Engineering'),
    viewport: TALL,
    annotations: [{ target: { text: 'Members', exact: true }, kind: 'arrow' }],
  },
  {
    id: 'request-rules',
    route: '/_/admin/access/admission',
    alt: 'The Request rules page lists two rules that run before authentication; callouts 1 and 2 mark them in the order that the proxy checks them.',
    annotations: [
      { target: { label: 'rule deny-anonymous-writes-downloads', exact: true }, kind: 'callout', label: '1', side: 'left' },
      { target: { label: 'rule block-scanner-network', exact: true }, kind: 'callout', label: '2', side: 'left' },
    ],
  },
  {
    id: 'login-iam',
    route: '/_/',
    auth: 'none',
    alt: 'The sign-in page of a proxy with IAM users asks for an access key ID and a secret access key; the box marks the two fields.',
    annotations: [
      { target: { union: [{ placeholder: 'Access Key ID' }, { placeholder: 'Secret Access Key' }] }, kind: 'box' },
    ],
  },
];
