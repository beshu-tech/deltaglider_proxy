// Jobs QA lc #19: typing an exclude glob replaced the default
// ".deltaglider/**", and the help text said the default protects the
// config-sync prefix, so it read like a risk. The planner skips the proxy's
// own keys in any case (is_internal_key in src/lifecycle/planner.rs); the
// help text says so.
import { readFileSync } from 'node:fs';
import { expect, test } from 'vitest';

test('the exclude-globs help says the proxy never acts on its own keys', () => {
  const src = readFileSync(new URL('../components/LifecycleRuleFields.tsx', import.meta.url), 'utf8');
  expect(src).toMatch(/never acts on its own \.deltaglider\/ and \.dg\/ keys, also when you replace the default pattern/);
  const planner = readFileSync(new URL('../../../../../src/lifecycle/planner.rs', import.meta.url), 'utf8');
  expect(planner).toMatch(/key\.starts_with\("\.deltaglider\/"\)/);
  expect(planner).toMatch(/key\.starts_with\("\.dg\/"\)/);
});
