/**
 * Access shots: credentials, users, groups, conditions, SSO, request rules
 * (docs: tutorials/secure-your-proxy, how-to/create-iam-users,
 * how-to/manage-iam-as-code, how-to/restrict-access-with-conditions,
 * how-to/set-up-sso, how-to/gate-requests-with-admission-rules,
 * explanation/security-model).
 */
import { request, type APIRequestContext, type Page } from '@playwright/test';
import type { Shot, Target } from '../shot';
import { BASE, ADMIN_PASSWORD, ENGINEERING, ENGINEERING_MEMBERS, USERS } from '../seed';

/** Select a row of a master-detail list (users, groups) by its name. */
async function pick(page: Page, name: string): Promise<void> {
  // A group row starts with its folder icon, whose label is part of the name.
  await page.getByRole('button', { name: new RegExp(`(^|\\s)${name}\\b`) }).first().click();
}

/** The user and group editors are taller than one screen. */
const TALL = { width: 1040, height: 1800 };

/** An option of the one open (visible) AntD select dropdown. */
const openOption = (page: Page, title: string) =>
  page.locator(`.ant-select-dropdown:not(.ant-select-dropdown-hidden) .ant-select-item-option[title="${title}"]`);

/** The card of the n-th permission rule of the open form (its WHERE label, three levels up). */
const ruleCard = (nth: number): Target => ({ css: `text="WHERE" >> nth=${nth} >> xpath=../../..` });

/** Type a resource pattern into a rule row and leave the field (it normalizes on blur). */
async function setResource(page: Page, nth: number, value: string): Promise<void> {
  const input = page.getByRole('combobox', { name: 'Resource pattern' }).nth(nth);
  await input.fill(value);
  await input.press('Tab');
}

/** No focus ring and no hover state: the form as it looks before the click. */
async function calm(page: Page): Promise<void> {
  await page.evaluate(() => (document.activeElement as HTMLElement | null)?.blur());
  await page.mouse.move(1, 1);
}

// ── SSO: the admin API state that the SSO shots need ─────────────────────
// The seed has no identity provider. These helpers add the example
// provider through the admin API, the same request that the form sends, and
// they are idempotent, because each shot runs once per theme.

const OKTA = {
  name: 'okta',
  provider_type: 'oidc',
  display_name: 'Okta',
  // A fake issuer: the form shots never sign anybody in.
  issuer_url: 'https://acme.okta.com/oauth2/default',
  client_id: '0oa1acmedeltaglider',
  client_secret: 'docs-okta-client-secret',
  scopes: 'openid email profile groups',
};

async function adminApi() {
  const ctx = await request.newContext({ baseURL: BASE, extraHTTPHeaders: { Origin: BASE } });
  const r = await ctx.post('/_/api/admin/login', { data: { password: ADMIN_PASSWORD } });
  if (!r.ok()) throw new Error(`access shots: admin login: HTTP ${r.status()}`);
  return ctx;
}

type Reply = { ok(): boolean; status(): number; json(): Promise<unknown> };
async function json<T>(p: Promise<Reply>, what: string): Promise<T> {
  const r = await p;
  if (!r.ok()) throw new Error(`access shots: ${what}: HTTP ${r.status()}`);
  return (await r.json()) as T;
}

type Provider = { id: number; name: string };
type Rule = { id: number };

/** Remove every identity provider (the "no provider yet" state). */
async function removeProviders(): Promise<void> {
  const ctx = await adminApi();
  const list = await json<Provider[]>(ctx.get('/_/api/admin/ext-auth/providers'), 'providers');
  for (const p of list) await ctx.delete(`/_/api/admin/ext-auth/providers/${p.id}`);
  await ctx.dispose();
}

/** The okta provider exists, and no mapping rule exists. */
async function oktaWithoutRules(): Promise<void> {
  const ctx = await adminApi();
  const list = await json<Provider[]>(ctx.get('/_/api/admin/ext-auth/providers'), 'providers');
  if (!list.some((p) => p.name === OKTA.name)) {
    await json(ctx.post('/_/api/admin/ext-auth/providers', { data: OKTA }), 'create okta');
  }
  const rules = await json<Rule[]>(ctx.get('/_/api/admin/ext-auth/mappings'), 'mappings');
  for (const r of rules) await ctx.delete(`/_/api/admin/ext-auth/mappings/${r.id}`);
  await ctx.dispose();
}

// ── "New" forms: the seed already holds the example user and group ──────
// A shot of the form that creates ci-uploader (or Engineering) must not show
// that user (or group) in the list already. Setup deletes it through the
// admin API, and the teardown creates it again as the seed did.

type Named = { id: number; name: string };

/** A state change whose reply body does not matter (DELETE may answer 204). */
async function sent(p: Promise<Reply>, what: string): Promise<void> {
  const r = await p;
  if (!r.ok()) throw new Error(`access shots: ${what}: HTTP ${r.status()}`);
}

async function withoutUser(page: Page, name: string): Promise<void> {
  const users = await json<Named[]>(page.request.get('/_/api/admin/users'), 'users');
  const u = users.find((x) => x.name === name);
  if (u) await sent(page.request.delete(`/_/api/admin/users/${u.id}`, { headers: { Origin: BASE } }), `delete ${name}`);
  await page.reload();
}

async function restoreUser(api: APIRequestContext, name: string): Promise<void> {
  const users = await json<Named[]>(api.get('/_/api/admin/users'), 'users');
  if (users.some((x) => x.name === name)) return;
  const def = USERS.find((x) => x.name === name)!;
  await sent(api.post('/_/api/admin/users', { data: def, headers: { Origin: BASE } }), `create ${name}`);
}

async function withoutEngineering(page: Page): Promise<void> {
  const groups = await json<Named[]>(page.request.get('/_/api/admin/groups'), 'groups');
  const g = groups.find((x) => x.name === ENGINEERING.name);
  if (g) await sent(page.request.delete(`/_/api/admin/groups/${g.id}`, { headers: { Origin: BASE } }), 'delete Engineering');
  await page.reload();
}

async function restoreEngineering(api: APIRequestContext): Promise<void> {
  const groups = await json<Named[]>(api.get('/_/api/admin/groups'), 'groups');
  if (groups.some((x) => x.name === ENGINEERING.name)) return;
  const users = await json<Named[]>(api.get('/_/api/admin/users'), 'users');
  const member_ids = ENGINEERING_MEMBERS.map((n) => users.find((u) => u.name === n)!.id);
  await sent(api.post('/_/api/admin/groups', { data: { ...ENGINEERING, member_ids }, headers: { Origin: BASE } }), 'create Engineering');
}

// ── Request rules ────────────────────────────────────────────────────────

/**
 * The state before the reader adds the example rule of the page: the
 * seeded `deny-anonymous-writes-downloads` is removed from the page (not
 * applied), so that the Add rule form accepts its name.
 */
async function withoutExampleRule(page: Page): Promise<void> {
  await page.getByRole('button', { name: 'More actions for rule deny-anonymous-writes-downloads' }).click();
  await page.getByRole('menuitem', { name: /Remove rule/ }).click();
  await page.getByRole('button', { name: 'Remove', exact: true }).click();
  await page.getByLabel('rule deny-anonymous-writes-downloads', { exact: true }).waitFor({ state: 'detached' });
}

async function fillExampleRule(page: Page): Promise<void> {
  await page.getByRole('button', { name: 'Add rule' }).click();
  const dialog = page.getByRole('dialog');
  await dialog.getByPlaceholder('e.g. deny-known-bad-ips').fill('deny-anonymous-writes-downloads');
  // A click on the label: the checkbox state follows the form state one render later.
  for (const m of ['PUT', 'POST', 'DELETE']) await dialog.locator('label.ant-checkbox-wrapper', { hasText: new RegExp(`^${m}$`) }).click();
  await dialog.getByPlaceholder('any bucket').fill('downloads');
  await dialog.locator('label', { hasText: /^Anonymous only$/ }).click();
  await dialog.locator('label', { hasText: /^Deny \(403\)$/ }).click();
  // The Source IPs box sizes itself to its text; it can measure itself while
  // the dialog opens and end up a few px off. A typed and deleted character
  // makes it measure again, once the layout is final.
  const ips = dialog.getByPlaceholder('203.0.113.5\n198.51.100.0/24');
  await ips.press('x');
  await ips.press('Backspace');
  await calm(page);
}

async function addExampleRule(page: Page): Promise<void> {
  await withoutExampleRule(page);
  await fillExampleRule(page);
  await page.getByRole('dialog').getByRole('button', { name: 'Add rule' }).click();
  await page.getByRole('dialog').waitFor({ state: 'detached' });
  await page.getByRole('button', { name: 'Review & apply' }).waitFor();
  await calm(page);
}

/** The whole Bootstrap SigV4 credentials card: the nearest rounded box around its title. */
const BOOTSTRAP_CARD: Target = {
  css: 'xpath=//*[text()="Bootstrap SigV4 credentials"]/ancestor::div[contains(@style, "border-radius")][1]',
};

export const ACCESS_SHOTS: Shot[] = [
  // First: the SSO shots below add an identity provider, which adds a button to this page.
  {
    id: 'login-iam',
    route: '/_/',
    auth: 'none',
    alt: 'The sign-in page of a proxy with IAM users asks for an access key ID and a secret access key; the box marks the two fields.',
    annotations: [
      { target: { union: [{ placeholder: 'Access Key ID' }, { placeholder: 'Secret Access Key' }] }, kind: 'box' },
    ],
  },
  // ── tutorials/secure-your-proxy ──
  {
    id: 'secure-credentials-mode',
    route: '/_/admin/access/credentials',
    alt: 'The Credentials & mode page; callout 1 marks Auto-detect (recommended) under S3 authentication mode, and callout 2 marks the Bootstrap SigV4 credentials card, which holds the access key ID of the proxy.',
    viewport: { width: 1040, height: 1300 },
    clip: { union: [{ text: 'S3 authentication mode', exact: true }, BOOTSTRAP_CARD] },
    clipPadding: 40,
    annotations: [
      { target: { css: 'label.ant-radio-wrapper:has-text("Auto-detect (recommended)")' }, kind: 'box', label: '1' },
      { target: BOOTSTRAP_CARD, kind: 'box', label: '2' },
    ],
  },
  // ── tutorials/secure-your-proxy + how-to/create-iam-users ──
  {
    id: 'iam-users-new-form',
    route: '/_/admin/access/users',
    alt: 'The Users page with the form of the new ci-uploader user; callout 1 marks Users in the sidebar, callout 2 the New button, callout 3 the Name field, callout 4 the rule that allows List, Read and Write on releases/firmware/*, and callout 5 the Create User button.',
    viewport: { width: 1040, height: 1200 },
    setup: async (page) => {
      await withoutUser(page, 'ci-uploader');
      await page.getByRole('button', { name: 'New User' }).click();
      await page.getByRole('textbox', { name: 'User name' }).fill('ci-uploader');
      await setResource(page, 0, 'releases/firmware/*');
      await page.getByRole('checkbox', { name: 'Write', exact: true }).click();
      await calm(page);
    },
    teardown: (api) => restoreUser(api, 'ci-uploader'),
    annotations: [
      // Not nav(): an unsaved form adds a marker to the sidebar entry's name.
      {
        target: { role: 'button', name: /(^|\s)Users\b/, within: { role: 'navigation', name: 'Admin navigation' } },
        kind: 'box',
        label: '1',
        side: 'right',
      },
      { target: { role: 'button', name: 'New User' }, kind: 'callout', label: '2', side: 'left' },
      { target: { role: 'textbox', name: 'User name' }, kind: 'callout', label: '3', side: 'left' },
      { target: ruleCard(0), kind: 'box', label: '4' },
      { target: { role: 'button', name: 'Create user' }, kind: 'callout', label: '5', side: 'left' },
    ],
  },
  // ── how-to/create-iam-users ──
  {
    id: 'iam-users-rotate',
    route: '/_/admin/access/users',
    alt: 'The form of the ci-uploader user after a click on Generate random secret; callout 1 marks the Generate random secret button, and callout 2 marks the Save button.',
    viewport: TALL,
    setup: async (page) => {
      await pick(page, 'ci-uploader');
      await page.getByTitle('Generate random secret').click();
      await calm(page);
    },
    clip: { union: [{ text: 'Edit: ci-uploader' }, { role: 'button', name: 'Save user' }] },
    clipPadding: 24,
    annotations: [
      { target: { css: '[title="Generate random secret"]' }, kind: 'box', label: '1', side: 'top' },
      { target: { role: 'button', name: 'Save user' }, kind: 'callout', label: '2', side: 'left' },
    ],
  },
  {
    id: 'iam-users-duplicate',
    route: '/_/admin/access/users',
    alt: 'The list of users; the box marks the Duplicate user with fresh credentials button on the ci-uploader row.',
    setup: async (page) => {
      await page.getByRole('button', { name: 'Duplicate ci-uploader with fresh credentials' }).hover();
    },
    clip: { css: '.dg-md-list' },
    clipPadding: 16,
    annotations: [
      { target: { role: 'button', name: 'Duplicate ci-uploader with fresh credentials' }, kind: 'box' },
    ],
  },
  {
    id: 'iam-groups-new-form',
    route: '/_/admin/access/groups',
    alt: 'The form of a new Engineering group; callout 1 marks the New button, callout 2 the Name field, callout 3 the rule that allows List and Read on releases/*, callout 4 the members dana and backup-bot, and callout 5 the Create Group button.',
    viewport: TALL,
    setup: async (page) => {
      await withoutEngineering(page);
      await page.getByRole('button', { name: 'New Group' }).click();
      await page.getByRole('textbox', { name: 'Group name' }).fill('Engineering');
      await page.getByPlaceholder('e.g. Development team access').fill('Firmware and platform engineers');
      await setResource(page, 0, 'releases/*');
      await page.getByRole('checkbox', { name: 'List', exact: true }).click();
      await page.getByRole('checkbox', { name: 'Read', exact: true }).click();
      await page.getByRole('checkbox', { name: 'dana' }).click();
      await page.getByRole('checkbox', { name: 'backup-bot' }).click();
      await calm(page);
    },
    teardown: restoreEngineering,
    clip: { union: [{ css: '.dg-md-list >> text="Groups"' }, { role: 'button', name: 'Create group' }] },
    clipPadding: 24,
    annotations: [
      { target: { role: 'button', name: 'New Group' }, kind: 'callout', label: '1', side: 'left' },
      { target: { role: 'textbox', name: 'Group name' }, kind: 'callout', label: '2', side: 'left' },
      { target: ruleCard(0), kind: 'box', label: '3' },
      {
        target: { union: [{ role: 'checkbox', name: 'dana' }, { role: 'checkbox', name: 'backup-bot' }] },
        kind: 'callout',
        label: '4',
        side: 'left',
      },
      { target: { role: 'button', name: 'Create group' }, kind: 'callout', label: '5', side: 'left' },
    ],
  },
  // ── how-to/restrict-access-with-conditions ──
  {
    id: 'user-ip-condition',
    route: '/_/admin/access/users',
    alt: 'The rule of the backup-bot user with its conditions open; callout 1 marks the backup-bot row, callout 2 the Conditions button, callout 3 the IP restriction field with 203.0.113.0/24, and callout 4 the Save button.',
    setup: (page) => pick(page, 'backup-bot'),
    viewport: TALL,
    clip: { union: [{ css: '.dg-md-list' }, { role: 'button', name: 'Save user' }] },
    clipPadding: 24,
    annotations: [
      { target: { role: 'button', name: /^backup-bot/ }, kind: 'box', label: '1', side: 'top' },
      { target: { role: 'button', name: 'Conditions' }, kind: 'callout', label: '2', side: 'left' },
      { target: { union: [{ text: /^IP restriction/ }, { label: /IP restriction/ }] }, kind: 'box', label: '3' },
      { target: { role: 'button', name: 'Save user' }, kind: 'callout', label: '4', side: 'left' },
    ],
  },
  {
    id: 'iam-conditions-list-prefix',
    route: '/_/admin/access/users',
    alt: 'A second rule of the dana user denies List on every bucket when the listed prefix matches .*; callout 1 marks Add Permission Rule, callout 2 the Deny setting, callout 3 the List prefix field, and callout 4 the Save button.',
    viewport: { width: 1040, height: 2200 },
    setup: async (page) => {
      await pick(page, 'dana');
      await page.getByRole('button', { name: 'Add Permission Rule' }).click();
      const card = page.locator('text="WHERE" >> nth=1 >> xpath=../../..');
      await card.getByText('Deny', { exact: true }).click();
      await setResource(page, 1, '*');
      await card.getByRole('checkbox', { name: 'List', exact: true }).click();
      await card.getByRole('button', { name: 'Conditions' }).click();
      const prefix = card.getByRole('combobox', { name: 'Prefix pattern' });
      await prefix.fill('.*');
      await prefix.press('Tab');
      await calm(page);
    },
    clip: { union: [ruleCard(1), { role: 'button', name: 'Save user' }] },
    clipPadding: 40,
    annotations: [
      { target: { role: 'button', name: 'Add Permission Rule' }, kind: 'callout', label: '1', side: 'left' },
      { target: { text: 'Deny', exact: true, within: ruleCard(1) }, kind: 'box', label: '2', side: 'right' },
      { target: { role: 'combobox', name: 'Prefix pattern' }, kind: 'box', label: '3' },
      { target: { role: 'button', name: 'Save user' }, kind: 'callout', label: '4', side: 'left' },
    ],
  },
  {
    id: 'iam-conditions-group-ip',
    route: '/_/admin/access/groups',
    alt: 'The rule of the Engineering group with its conditions open; callout 1 marks the Conditions button, callout 2 the IP restriction field with 203.0.113.0/24, and callout 3 the Save button.',
    viewport: TALL,
    setup: async (page) => {
      await pick(page, 'Engineering');
      await page.getByRole('button', { name: 'Conditions' }).click();
      await page.getByLabel(/IP restriction/).fill('203.0.113.0/24');
      await calm(page);
    },
    clip: { union: [{ text: 'Edit: Engineering' }, { role: 'button', name: 'Save group' }] },
    clipPadding: 24,
    annotations: [
      { target: { role: 'button', name: 'Conditions' }, kind: 'callout', label: '1', side: 'left' },
      { target: { union: [{ text: /^IP restriction/ }, { label: /IP restriction/ }] }, kind: 'box', label: '2' },
      { target: { role: 'button', name: 'Save group' }, kind: 'callout', label: '3', side: 'left' },
    ],
  },
  // ── how-to/manage-iam-as-code ──
  {
    id: 'iam-code-export-menu',
    route: '/_/admin/access/users',
    alt: 'The account menu is open; the box marks Export full IAM (YAML), which downloads the users, groups, providers and mapping rules with their live secrets.',
    setup: async (page) => {
      await page.getByRole('button', { name: /^Account menu/ }).click();
      await page.getByRole('menuitem', { name: 'Export full IAM (YAML)' }).waitFor();
      await page.mouse.move(1, 1);
    },
    clip: { css: '.account-menu-panel' },
    clipPadding: 24,
    annotations: [{ target: { role: 'menuitem', name: 'Export full IAM (YAML)' }, kind: 'box' }],
  },
  // ── how-to/set-up-sso ──
  {
    id: 'sso-add-provider',
    route: '/_/admin/access/external-auth',
    alt: 'The External authentication page with the form of a new Okta provider; callout 1 marks the Add provider button, callout 2 the provider fields from Display Name to Scopes, and callout 3 the Create button.',
    viewport: TALL,
    setup: async (page) => {
      await removeProviders();
      await page.reload();
      await page.getByRole('button', { name: 'Add provider' }).click();
      await page.getByLabel('Display Name').fill(OKTA.display_name);
      await page.getByLabel('Provider Name (unique identifier)').fill(OKTA.name);
      await page.getByLabel('Issuer URL').fill(OKTA.issuer_url);
      await page.getByLabel('Client ID').fill(OKTA.client_id);
      await page.getByLabel('Client Secret').fill(OKTA.client_secret);
      await page.getByLabel('Scopes').fill(OKTA.scopes);
      await calm(page);
    },
    clip: { union: [{ role: 'button', name: 'Add provider' }, { label: 'Scopes' }, { role: 'button', name: 'Test Connection' }] },
    clipPadding: 32,
    annotations: [
      { target: { role: 'button', name: 'Add provider' }, kind: 'box', label: '1', side: 'bottom' },
      { target: { union: [{ text: 'Display Name', exact: true }, { label: 'Scopes' }] }, kind: 'box', label: '2' },
      { target: { role: 'button', name: 'Create', exact: true }, kind: 'callout', label: '3', side: 'left' },
    ],
  },
  {
    id: 'sso-mapping-rule',
    route: '/_/admin/access/external-auth',
    alt: 'A new mapping rule assigns everyone whose groups claim contains engineering to the Engineering group; callout 1 marks the Add Rule button, callout 2 the rule, and callout 3 the Save Rules button.',
    viewport: TALL,
    setup: async (page) => {
      await oktaWithoutRules();
      await page.reload();
      await page.getByRole('button', { name: 'Add Rule' }).click();
      await page.getByRole('combobox', { name: 'Match type' }).click();
      await openOption(page, 'Claim value').click();
      await page.getByRole('textbox', { name: 'Claim field' }).fill('groups');
      await page.getByRole('textbox', { name: 'Match value' }).fill('engineering');
      await page.getByRole('combobox', { name: 'Provider' }).click();
      await openOption(page, 'Okta').click();
      await calm(page);
    },
    clip: { union: [{ text: 'Allowed Users & Group Assignment', exact: true }, { role: 'button', name: 'Save Rules' }] },
    clipPadding: 60,
    annotations: [
      { target: { role: 'button', name: 'Add Rule' }, kind: 'callout', label: '1', side: 'left' },
      { target: { css: 'text="When" >> xpath=..' }, kind: 'box', label: '2' },
      { target: { role: 'button', name: 'Save Rules' }, kind: 'callout', label: '3', side: 'left' },
    ],
  },
  {
    id: 'sso-login-button',
    route: '/_/',
    auth: 'none',
    alt: 'The sign-in page of a proxy with the Okta provider; the arrow points at the Sign in with Okta button above the link to sign in with credentials.',
    setup: async (page) => {
      await oktaWithoutRules();
      await page.reload();
      await page.getByRole('link', { name: 'Sign in with Okta' }).waitFor();
    },
    clip: { union: [{ role: 'link', name: 'Sign in with Okta' }, { role: 'button', name: 'Sign in with credentials instead' }] },
    clipPadding: 64,
    annotations: [{ target: { role: 'link', name: 'Sign in with Okta' }, kind: 'arrow', side: 'right' }],
  },
  // ── how-to/gate-requests-with-admission-rules ──
  {
    id: 'admission-add-rule',
    route: '/_/admin/access/admission',
    alt: 'The Add request rule form holds the rule deny-anonymous-writes-downloads; callout 1 marks the Add rule button of the page, callout 2 the conditions, callout 3 the Deny (403) action under Then, and callout 4 the Add rule button of the form.',
    viewport: { width: 1040, height: 1000 },
    setup: async (page) => {
      await withoutExampleRule(page);
      await fillExampleRule(page);
    },
    annotations: [
      { target: { role: 'button', name: 'Add rule', nth: 0 }, kind: 'callout', label: '1', side: 'left' },
      {
        target: { union: [{ text: /^When a request matches/ }, { role: 'radio', name: 'Anonymous only' }] },
        kind: 'box',
        label: '2',
      },
      { target: { css: '.ant-modal label.ant-radio-wrapper:has-text("Deny (403)")' }, kind: 'box', label: '3', side: 'top' },
      { target: { role: 'button', name: 'Add rule', within: { role: 'dialog' } }, kind: 'box', label: '4', side: 'top' },
    ],
  },
  {
    id: 'admission-reorder-apply',
    route: '/_/admin/access/admission',
    alt: 'The new rule deny-anonymous-writes-downloads is at the end of the list, and the page holds an unsaved change; callout 5 marks the drag handle of the new rule, and callout 6 marks the Review & apply button.',
    setup: addExampleRule,
    annotations: [
      {
        target: { role: 'button', name: 'drag to reorder', within: { label: 'rule deny-anonymous-writes-downloads', exact: true } },
        kind: 'callout',
        label: '5',
        side: 'left',
      },
      { target: { role: 'button', name: 'Review & apply' }, kind: 'box', label: '6', side: 'right' },
    ],
    clip: { union: [{ text: 'Your rules', exact: true }, { role: 'button', name: 'Add rule' }, { role: 'button', name: 'Review & apply' }] },
    clipPadding: 56,
  },
  {
    id: 'admission-apply-dialog',
    route: '/_/admin/access/admission',
    alt: 'The review dialog shows the change to the list of request rules; the box marks the Apply and Persist button.',
    setup: async (page) => {
      await addExampleRule(page);
      await page.getByRole('button', { name: 'Review & apply' }).dispatchEvent('click');
      await page.getByTestId('apply-dialog-confirm').waitFor();
    },
    clip: { role: 'dialog' },
    clipPadding: 16,
    annotations: [{ target: { testId: 'apply-dialog-confirm' }, kind: 'box' }],
  },
  {
    id: 'admission-trace-deny',
    route: '/_/admin/diagnostics/trace',
    alt: 'The request rule tester shows that an anonymous PUT of downloads/public/installer.zip is denied; the box marks the decision and the rule deny-anonymous-writes-downloads that made it.',
    setup: async (page) => {
      await page.locator('label.ant-radio-button-wrapper', { hasText: /^PUT$/ }).click();
      await page.getByPlaceholder('/my-bucket/some/key').fill('/downloads/public/installer.zip');
      await page.getByRole('button', { name: 'Test request' }).click();
      await page.getByText('Reason path').waitFor();
      await page.getByText('Decision', { exact: true }).evaluate((el) => el.scrollIntoView({ block: 'start' }));
    },
    clip: { union: [{ text: 'Decision', exact: true }, { role: 'button', name: 'Copy as JSON' }] },
    clipPadding: 48,
    annotations: [{ target: { css: 'text="by rule" >> xpath=..' }, kind: 'box' }],
  },
  // ── explanation/security-model ──
  {
    id: 'request-rules',
    route: '/_/admin/access/admission',
    alt: 'The Request rules page lists two rules that run before authentication; callouts 1 and 2 mark them in the order that the proxy checks them.',
    annotations: [
      { target: { label: 'rule deny-anonymous-writes-downloads', exact: true }, kind: 'callout', label: '1', side: 'left' },
      { target: { label: 'rule block-scanner-network', exact: true }, kind: 'callout', label: '2', side: 'left' },
    ],
  },
];
