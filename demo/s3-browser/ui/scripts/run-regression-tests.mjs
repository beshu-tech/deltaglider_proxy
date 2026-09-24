/**
 * Runs EVERY `test:*` script in package.json. CI calls this instead of
 * listing scripts by hand, so a new regression test cannot be added to
 * package.json and silently never run.
 */
import { spawnSync } from 'node:child_process';
import { readFile } from 'node:fs/promises';

const pkg = JSON.parse(await readFile(new URL('../package.json', import.meta.url), 'utf8'));
const names = Object.keys(pkg.scripts).filter((n) => n.startsWith('test:') && n !== 'test:all');
const failed = [];
for (const name of names) {
  const r = spawnSync('npm', ['run', '-s', name], { stdio: 'inherit' });
  if (r.status !== 0) failed.push(name);
}
console.log(`\n${names.length - failed.length}/${names.length} regression scripts passed`);
if (failed.length > 0) {
  console.error(`FAILED: ${failed.join(', ')}`);
  process.exit(1);
}
