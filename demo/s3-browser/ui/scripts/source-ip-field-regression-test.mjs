import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import ts from 'typescript';

const source = await readFile(new URL('../src/sourceIpField.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ES2020, target: ts.ScriptTarget.ES2020 },
  fileName: 'sourceIpField.ts',
});
const { sourceIpMatch, sourceIpText, parseSourceIpLines, sourceIpProblem } = await import(
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

// A pasted list: commas and spaces separate entries too.
assert.deepEqual(parseSourceIpLines('203.0.113.5, 10.0.0.0/8 2001:db8::1'), ['203.0.113.5', '10.0.0.0/8', '2001:db8::1']);
assert.deepEqual(sourceIpMatch('203.0.113.5,198.51.100.0/24'), { source_ip_list: ['203.0.113.5', '198.51.100.0/24'] });

// Client-side validation, with the line number (issue #92 review).
for (const ok of [
  '',
  '203.0.113.5',
  '0.0.0.0/0',
  '10.0.0.0/8\n192.168.1.1/32',
  '2001:db8::1',
  '::',
  '::1/128',
  '2001:db8::/32',
  '::ffff:192.0.2.1',
  'fe80::1, 10.1.2.3',
]) {
  assert.equal(sourceIpProblem(ok), null, `valid: ${JSON.stringify(ok)}`);
}
assert.equal(sourceIpProblem('10.0.0.1\nfoo'), 'Line 2: "foo" is not an IP address or a network such as 10.0.0.0/8.');
assert.match(sourceIpProblem('256.1.1.1'), /^Line 1: .*not an IP address/);
assert.match(sourceIpProblem('1.2.3'), /not an IP address/);
assert.match(sourceIpProblem('01.2.3.4'), /not an IP address/, 'leading zeros are ambiguous (octal)');
assert.match(sourceIpProblem('10.0.0.0/33'), /between \/0 and \/32/);
assert.match(sourceIpProblem('2001:db8::/129'), /between \/0 and \/128/);
assert.match(sourceIpProblem('10.0.0.0/008'), /between \/0 and \/32/, 'leading zeros in the size: the server rejects them');
assert.match(sourceIpProblem('10.0.0.0/08'), /between/);
assert.equal(sourceIpProblem('10.0.0.0/0'), null);
assert.match(sourceIpProblem('10.0.0.0/x'), /between \/0 and \/32/);
assert.match(sourceIpProblem('10.0.0.0/8/8'), /not an IP address/);
assert.match(sourceIpProblem('2001:db8:::1'), /not an IP address/);
assert.match(sourceIpProblem('fe80::1%eth0'), /not an IP address/, 'no zone ids');
// An over-long entry (the schema's max(64)) gets a message with its line.
assert.match(sourceIpProblem('1.2.3.4\n\n' + 'a'.repeat(70)), /^Line 3: "a{40}…" is longer than 64 characters\.$/);
// Entry count cap.
const many = Array.from({ length: 4097 }, (_, i) => `10.${(i >> 8) & 255}.${i & 255}.1`).join('\n');
assert.match(sourceIpProblem(many), /at most 4096/);

console.log('source-ip field regression checks passed');
