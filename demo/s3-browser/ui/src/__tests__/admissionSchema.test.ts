// The admission rule schema speaks in the one copy style of the admin forms:
// every message is a full sentence with a capital and a period.
import { expect, test } from 'vitest';
import { admissionBlockSchema } from '../schemas/admissionSchema';

const valid = { name: 'deny-bots', match: {}, action: 'deny' as const };

function messages(input: unknown): string[] {
  const r = admissionBlockSchema.safeParse(input);
  return r.success ? [] : r.error.issues.map((i) => i.message);
}

test('the name messages', () => {
  expect(messages({ ...valid, name: '' })).toContain('Enter a name.');
  expect(messages({ ...valid, name: 'a'.repeat(129) })).toEqual(['Use 128 characters or fewer.']);
  expect(messages({ ...valid, name: 'no spaces' })).toEqual(['Use letters, digits and _ : . - only.']);
});

test('every message is a sentence', () => {
  const all = [
    ...messages({ ...valid, name: '' }),
    ...messages({ ...valid, match: { source_ip: ' ' } }),
    ...messages({ ...valid, match: { source_ip_list: [' '] } }),
    ...messages({ ...valid, match: { source_ip: '10.0.0.1', source_ip_list: ['10.0.0.2'] } }),
    ...messages({ ...valid, match: { bucket: ' ' } }),
    ...messages({ ...valid, match: { path_glob: ' ' } }),
    ...messages({ ...valid, match: { config_flag: ' ' } }),
    ...messages({ ...valid, action: { type: 'reject', status: 200 } }),
  ];
  expect(all.length).toBeGreaterThanOrEqual(8);
  for (const m of all) expect(m).toMatch(/^[A-Z].*\.$/);
});
