import { test, expect, type Page } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { S3Client, CreateBucketCommand, PutObjectCommand } from '@aws-sdk/client-s3';
const AXE = readFileSync(process.env.HOME + '/.cache/claude-work/browser-review/axe/node_modules/axe-core/axe.min.js', 'utf8');
const AK = process.env.REVIEW_ACCESS_KEY!, SK = process.env.REVIEW_SECRET_KEY!;
async function run(page: Page, name: string) {
  await page.addScriptTag({ content: AXE });
  const res = await page.evaluate(async () => {
    // @ts-expect-error axe global
    const r = await axe.run(document, { resultTypes: ['violations'] });
    return r.violations.map((v: any) => ({ id: v.id, impact: v.impact, n: v.nodes.length, nodes: v.nodes.slice(0, 4).map((n: any) => n.target.join(' ') + ' :: ' + (n.failureSummary || '').split('\n').slice(1, 2).join(' ')) }));
  });
  console.log(`== ${name}`);
  for (const v of res) console.log(` ${v.impact} ${v.id} x${v.n}\n   ${v.nodes.join('\n   ')}`);
}
test('axe', async ({ page }) => {
  test.setTimeout(300_000);
  const c = new S3Client({ endpoint: process.env.PLAYWRIGHT_BASE_URL, region: 'us-east-1', forcePathStyle: true, credentials: { accessKeyId: AK, secretAccessKey: SK } });
  await c.send(new CreateBucketCommand({ Bucket: 'releases' })).catch(() => undefined);
  await c.send(new PutObjectCommand({ Bucket: 'releases', Key: 'firmware/README.txt', Body: 'hi' }));
  await page.goto('/_/'); await page.waitForTimeout(1000); await run(page, 'login');
  await page.goto('/_/admin');
  await page.getByPlaceholder('Access Key ID').fill(AK);
  await page.getByPlaceholder('Secret Access Key').fill(SK);
  await page.getByRole('button', { name: 'Sign in' }).click();
  await expect(page.getByText('ADMIN SETTINGS', { exact: false })).toBeVisible({ timeout: 30_000 });
  for (const theme of (process.env.THEMES ?? 'dark,light').split(',')) {
    await page.evaluate((t) => localStorage.setItem('dg-theme', t), theme);
    for (const [p, n] of [['/_/browse/releases/firmware/', 'browser'], ['/_/admin/dashboard', 'dashboard'], ['/_/admin/access/users', 'users'], ['/_/admin/jobs', 'jobs'], ['/_/admin/storage/buckets', 'buckets'], ['/_/upload', 'upload']]) {
      await page.goto(p); await page.waitForLoadState('networkidle'); await page.waitForTimeout(1200);
      await run(page, `${theme} ${n}`);
    }
  }
});
