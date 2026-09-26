/** src/docSearchText.ts */
import assert from 'node:assert/strict';
import { test } from 'vitest';
import { docSearchSnippet, docSearchText } from '../docSearchText';

const changelog = `<!-- GENERATED FILE — do not edit.
     Source of truth: CHANGELOG.md at the repo root. -->

# Changelog

Every released version, newest first.

## v1.19.0

A bucket marked \`replication_target_only\` refuses **client** writes.
![Jobs screen](/_/screenshots/jobs.jpg)

\`\`\`yaml
storage: {}
\`\`\`
`;

test('search text drops the GENERATED comment, the h1 and code blocks, keeps inline code', () => {
  // Explore finding 19: the Changelog hit read "<!- GENERATED FILE — do not
  // edit…", and a search for an identifier in backticks found no snippet.
  const t = docSearchText(changelog);
  assert.ok(!t.includes('GENERATED'), t);
  assert.ok(!t.startsWith('Changelog'), t);
  assert.ok(t.includes('replication_target_only'), t);
  assert.ok(t.includes('refuses client writes'), t);
  assert.ok(!t.includes('storage: {}'), t);
  assert.ok(!t.includes('screenshots'), t);
});

test('the snippet is centred on the match, for any query word that matches', () => {
  const s = docSearchSnippet(docSearchText(changelog), 'zzz replication_target_only');
  assert.match(s, /replication_target_only/);
  assert.ok(!s.includes('GENERATED'));
});

test('underscores inside words survive; emphasis underscores do not', () => {
  assert.equal(docSearchText('Set _max_delta_ratio_ to _off_.'), 'Set max_delta_ratio to off.');
});
