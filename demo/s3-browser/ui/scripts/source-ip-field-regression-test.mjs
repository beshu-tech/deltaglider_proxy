import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

const source = await readFile(new URL('../src/sourceIpField.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'sourceIpField.ts',
});
const { sourceIpMatch, sourceIpText, parseSourceIpLines } = await import(
  `data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`
);

// Lines are trimmed; blank lines are dropped.
assert.deepEqual(parseSourceIpLines(' 203.0.113.5 \n\n10.0.0.0/8\n'), ['203.0.113.5', '10.0.0.0/8']);

// Empty field → no source-IP condition at all.
assert.deepEqual(sourceIpMatch('  \n'), {});
// One plain address → source_ip.
assert.deepEqual(sourceIpMatch('203.0.113.5'), { source_ip: '203.0.113.5' });
assert.deepEqual(sourceIpMatch('2001:db8::1'), { source_ip: '2001:db8::1' });
// A network, even alone → source_ip_list (source_ip takes no CIDR).
assert.deepEqual(sourceIpMatch('10.0.0.0/8'), { source_ip_list: ['10.0.0.0/8'] });
// Several entries → source_ip_list.
assert.deepEqual(sourceIpMatch('203.0.113.5\n198.51.100.0/24'), {
  source_ip_list: ['203.0.113.5', '198.51.100.0/24'],
});
// Never both keys: the two were mutually exclusive, the server rejects both.
for (const t of ['1.2.3.4', '1.2.3.4\n5.6.7.8', '1.0.0.0/8']) {
  const m = sourceIpMatch(t);
  assert.ok(!(m.source_ip && m.source_ip_list), t);
}

// Loading a rule shows its entries, whichever key it used.
assert.equal(sourceIpText({ source_ip: '203.0.113.5' }), '203.0.113.5');
assert.equal(sourceIpText({ source_ip_list: ['a', 'b'] }), 'a\nb');
assert.equal(sourceIpText({}), '');

// An unchanged single-entry list keeps its original key (no YAML churn).
assert.deepEqual(sourceIpMatch('203.0.113.5', { source_ip_list: ['203.0.113.5'] }), {
  source_ip_list: ['203.0.113.5'],
});
assert.deepEqual(sourceIpMatch('203.0.113.5', { source_ip: '203.0.113.5' }), { source_ip: '203.0.113.5' });
// A changed value follows the normal rule.
assert.deepEqual(sourceIpMatch('203.0.113.6', { source_ip_list: ['203.0.113.5'] }), { source_ip: '203.0.113.6' });

console.log('source-ip field regression checks passed');
