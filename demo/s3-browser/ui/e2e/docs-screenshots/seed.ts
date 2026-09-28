/**
 * Seeds the fixed example cast (repo CLAUDE.md) into a proxy started by
 * `scripts/lib/e2e-proxy.sh docs`, through the proxy's own APIs only: the
 * admin API for backends, buckets, bucket policies, jobs and IAM, and the S3
 * API for objects. Nothing is written onto a backend directly, so every
 * object carries the metadata that the write path gives it.
 *
 * The one step outside the proxy is `resetMinio`: it empties the MinIO
 * buckets that a previous run filled, so every run starts from the same state.
 */
import { request, type APIRequestContext } from '@playwright/test';
import {
  CreateBucketCommand,
  DeleteBucketCommand,
  DeleteObjectsCommand,
  ListBucketsCommand,
  ListObjectsV2Command,
  PutObjectCommand,
  S3Client,
} from '@aws-sdk/client-s3';

export const BASE = process.env.PLAYWRIGHT_BASE_URL ?? 'http://127.0.0.1:19077';
export const ADMIN_PASSWORD = 'testpass';
const ACCESS_KEY = process.env.E2E_ACCESS_KEY ?? 'qa-admin-key';
const SECRET_KEY = process.env.E2E_SECRET_KEY ?? 'qa-admin-secret-0123456789';
const MINIO = process.env.DOCS_MINIO_ENDPOINT ?? 'http://127.0.0.1:9000';
const MINIO_KEY = process.env.DOCS_MINIO_KEY ?? 'minioadmin';
const MINIO_SECRET = process.env.DOCS_MINIO_SECRET ?? 'minioadmin';

/** The MinIO buckets behind the two S3 backends. */
const MINIO_BUCKETS = ['downloads', 'releases', 'releases-dr'];

async function admin(): Promise<APIRequestContext> {
  // Same-origin header: the admin API refuses cross-origin state changes.
  const ctx = await request.newContext({ baseURL: BASE, extraHTTPHeaders: { Origin: BASE } });
  await ok(ctx.post('/_/api/admin/login', { data: { password: ADMIN_PASSWORD } }), 'login');
  return ctx;
}

async function ok(p: ReturnType<APIRequestContext['get']>, what: string): Promise<unknown> {
  const r = await p;
  if (!r.ok()) throw new Error(`seed: ${what}: HTTP ${r.status()} ${await r.text()}`);
  const t = await r.text();
  return t ? JSON.parse(t) : null;
}

function s3(endpoint: string, key: string, secret: string): S3Client {
  return new S3Client({
    endpoint,
    region: 'us-east-1',
    forcePathStyle: true,
    credentials: { accessKeyId: key, secretAccessKey: secret },
  });
}

export async function resetMinio(): Promise<void> {
  const c = s3(MINIO, MINIO_KEY, MINIO_SECRET);
  // The S3 backends list every bucket that the MinIO key can see; a foreign
  // bucket would show up in the Buckets page and the browser.
  const all = (await c.send(new ListBucketsCommand({}))).Buckets ?? [];
  const foreign = all.map((b) => b.Name!).filter((n) => !MINIO_BUCKETS.includes(n));
  if (foreign.length > 0) {
    throw new Error(`seed: MinIO at ${MINIO} holds buckets outside the docs cast (${foreign.join(', ')}); use a fresh MinIO`);
  }
  for (const b of MINIO_BUCKETS) {
    for (;;) {
      let page;
      try {
        page = await c.send(new ListObjectsV2Command({ Bucket: b }));
      } catch {
        break; // no such bucket
      }
      const keys = (page.Contents ?? []).map((o) => ({ Key: o.Key! }));
      if (keys.length === 0) break;
      await c.send(new DeleteObjectsCommand({ Bucket: b, Delete: { Objects: keys } }));
    }
    await c.send(new DeleteBucketCommand({ Bucket: b })).catch(() => undefined);
  }
  c.destroy();
}

/** Deterministic bytes (mulberry32), so the delta savings are the same on every run. */
function bytes(seed: number, n: number): Buffer {
  let a = seed >>> 0;
  const out = Buffer.alloc(n);
  for (let i = 0; i < n; i++) {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    out[i] = (t ^ (t >>> 14)) & 0xff;
  }
  return out;
}

/** Firmware build `v` of a 3 MiB image: the same base, a small patched region per version. */
function firmware(v: number): Buffer {
  const img = bytes(3000, 3 * 1024 * 1024);
  bytes(4000 + v, 24 * 1024).copy(img, 512 * 1024 + v * 64 * 1024);
  return img;
}

const MB = 1024 * 1024;

const OBJECTS: { bucket: string; key: string; body: () => Buffer; type?: string }[] = [
  ...[0, 1, 2, 3].map((v) => ({
    bucket: 'releases',
    key: `firmware/widget-3000/fw-1.4.${v}.tar`,
    body: () => firmware(v),
    type: 'application/x-tar',
  })),
  {
    bucket: 'releases',
    key: 'reports/build-notes.md',
    type: 'text/markdown',
    body: () =>
      Buffer.from(
        '# widget-3000 firmware 1.4.3\n\n- Fixes the watchdog reset on cold boot.\n- Adds the fan curve for the 2026 chassis.\n- Signed with the release key of September 2026.\n',
      ),
  },
  {
    bucket: 'downloads',
    key: 'public/installer.sh',
    type: 'text/x-shellscript',
    body: () => Buffer.from('#!/bin/sh\nset -eu\necho "Installing widget-3000 tools"\n'),
  },
  { bucket: 'downloads', key: 'public/widget-3000-manual.pdf', type: 'application/pdf', body: () => bytes(7, 2 * MB) },
  ...['2026-09-25', '2026-09-26', '2026-09-27'].map((d, i) => ({
    bucket: 'db-archive',
    key: `nightly/${d}.dump`,
    body: () => {
      const b = bytes(9000, 4 * MB);
      bytes(9100 + i, 128 * 1024).copy(b, MB + i * 256 * 1024);
      return b;
    },
  })),
];

const USERS = [
  {
    name: 'ci-uploader',
    access_key_id: 'AKIADOCSCIUPLOADER01',
    secret_access_key: 'docs-ci-uploader-secret-0000000000000000',
    permissions: [{ effect: 'Allow', actions: ['read', 'write', 'list'], resources: ['releases/firmware/*'] }],
  },
  {
    name: 'backup-bot',
    access_key_id: 'AKIADOCSBACKUPBOT001',
    secret_access_key: 'docs-backup-bot-secret-00000000000000000',
    permissions: [
      {
        effect: 'Allow',
        actions: ['write', 'list'],
        resources: ['db-archive/*'],
        conditions: { IpAddress: { 'aws:SourceIp': '203.0.113.0/24' } },
      },
    ],
  },
  {
    name: 'dana',
    access_key_id: 'AKIADOCSDANA00000001',
    secret_access_key: 'docs-dana-secret-000000000000000000000000',
    permissions: [{ effect: 'Allow', actions: ['read', 'list'], resources: ['*'] }],
  },
];

async function iamVersion(ctx: APIRequestContext): Promise<number> {
  const v = (await ok(ctx.get('/_/api/admin/iam/version'), 'iam version')) as { version: number };
  return v.version;
}

async function waitFor<T>(what: string, probe: () => Promise<T | undefined>, ms = 60_000): Promise<T> {
  const end = Date.now() + ms;
  for (;;) {
    const v = await probe();
    if (v !== undefined) return v;
    if (Date.now() > end) throw new Error(`seed: timed out waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 250));
  }
}

export async function seed(): Promise<void> {
  {
    // Seeded already (a worker restarted after a failed shot): keep the state.
    const ctx = await admin();
    const b = (await ok(ctx.get('/_/api/admin/backends'), 'backends')) as { backends: { name: string }[] };
    await ctx.dispose();
    if (b.backends.some((x) => x.name === 'aws-dr')) return;
  }
  await resetMinio();
  const minio = s3(MINIO, MINIO_KEY, MINIO_SECRET);
  for (const b of ['releases', 'releases-dr']) await minio.send(new CreateBucketCommand({ Bucket: b }));
  minio.destroy();

  const ctx = await admin();
  const s3Backend = (name: string, region: string, setDefault: boolean) =>
    ok(
      ctx.post('/_/api/admin/backends', {
        data: {
          name,
          type: 's3',
          endpoint: MINIO,
          region,
          force_path_style: true,
          access_key_id: MINIO_KEY,
          secret_access_key: MINIO_SECRET,
          set_default: setDefault,
        },
      }),
      `backend ${name}`,
    );
  await s3Backend('hetzner-fsn1', 'fsn1', true);
  await s3Backend('aws-dr', 'eu-west-1', false);
  for (const [name, backend] of [
    ['releases', 'hetzner-fsn1'],
    ['releases-dr', 'aws-dr'],
    ['db-archive', 'local-disk'],
  ]) {
    await ok(ctx.post('/_/api/admin/buckets', { data: { name, backend_name: backend } }), `bucket ${name}`);
  }
  // `downloads` is created the way a client creates a bucket, so it has no
  // explicit route and follows the default backend (hetzner-fsn1): the
  // route-a-bucket shots route it to local-disk.
  const c = s3(BASE, ACCESS_KEY, SECRET_KEY);
  await c.send(new CreateBucketCommand({ Bucket: 'downloads' }));
  await ok(
    ctx.put('/_/api/admin/config/section/storage', {
      data: {
        buckets: {
          downloads: { public_prefixes: ['public/'] },
          releases: { quota_bytes: 50 * 1024 * 1024 * 1024 },
        },
        replication: {
          enabled: true,
          rules: [
            {
              name: 'releases-to-dr',
              enabled: true,
              source: { bucket: 'releases', prefix: '' },
              destination: { bucket: 'releases-dr', prefix: '' },
              interval: '24h',
            },
          ],
        },
        lifecycle: {
          enabled: true,
          rules: [
            {
              name: 'expire-old-downloads',
              enabled: true,
              bucket: 'downloads',
              prefix: '',
              action: 'delete',
              expire_after: '30d',
            },
          ],
        },
      },
    }),
    'storage section',
  );

  // Objects: with the bootstrap SigV4 pair, before IAM users exist.
  for (const o of OBJECTS) {
    await c.send(new PutObjectCommand({ Bucket: o.bucket, Key: o.key, Body: o.body(), ContentType: o.type }));
  }
  c.destroy();

  // IAM: users with fixed keys (the users page shows the access key id), then the group.
  const ids: Record<string, number> = {};
  for (const u of USERS) {
    const before = await iamVersion(ctx);
    const r = (await ok(ctx.post('/_/api/admin/users', { data: u }), `user ${u.name}`)) as { id: number };
    ids[u.name] = r.id;
    await waitFor('iam rebuild', async () => ((await iamVersion(ctx)) > before ? true : undefined));
  }
  await ok(
    ctx.post('/_/api/admin/groups', {
      data: {
        name: 'Engineering',
        description: 'Firmware and platform engineers',
        permissions: [{ effect: 'Allow', actions: ['read', 'list'], resources: ['releases/*'] }],
        member_ids: [ids['dana'], ids['backup-bot']],
      },
    }),
    'group Engineering',
  );

  // Request rules (admission): checked before authentication, first match wins.
  await ok(
    ctx.put('/_/api/admin/config/section/admission', {
      data: {
        blocks: [
          {
            name: 'deny-anonymous-writes-downloads',
            match: { method: ['PUT', 'POST', 'DELETE'], bucket: 'downloads', authenticated: false },
            action: 'deny',
          },
          {
            name: 'block-scanner-network',
            match: { source_ip_list: ['198.51.100.0/24'] },
            action: { type: 'reject', status: 403, message: 'This network is blocked.' },
          },
        ],
      },
    }),
    'admission section',
  );

  // One finished replication run, so the Jobs screen has history.
  await ok(ctx.post('/_/api/admin/jobs/replication:releases-to-dr/run-now'), 'replication run-now');
  await waitFor('replication run', async () => {
    const runs = (await ok(ctx.get('/_/api/admin/jobs/replication:releases-to-dr/runs'), 'runs')) as {
      runs?: { status: string }[];
    };
    return runs.runs?.some((r) => r.status !== 'running') ? true : undefined;
  });
  await ctx.dispose();
}
