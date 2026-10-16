/**
 * The rule filter section: per-glob match counts from the server's preview,
 * a suggestion for a glob written without the prefix, the summary line, and
 * the wide preview window with the keys and a key probe.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import RuleFilterFields from '../components/RuleFilterFields';
import type { FilterPreview } from '../adminApi/jobs';
import { json, mockFetch, type RecordedRequest } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const PREVIEW = '/_/api/admin/jobs/filter-preview';

const sample: FilterPreview = {
  sampled: 3,
  truncated: false,
  included: 1,
  excluded: 1,
  not_included: 0,
  internal: 0,
  globs: [
    { glob: '*.tmp', list: 'exclude', matches: 1 },
    { glob: 'builds/**', list: 'exclude', matches: 0, suggestion: 'ror/builds/**' },
  ],
  keys: [
    { key: 'ror/a.zip', verdict: 'included' },
    { key: 'ror/b.tmp', verdict: 'excluded', glob: '*.tmp' },
  ],
};

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('POST', PREVIEW, (req: RecordedRequest) => {
    const body = req.body as { keys?: string[] };
    if (body.keys) {
      return json({ ...sample, sampled: 1, keys: [{ key: body.keys[0], verdict: 'excluded', glob: '*.tmp' }] });
    }
    return json(sample);
  });
});
afterEach(() => {
  vi.unstubAllGlobals();
});

function section(onChange = vi.fn()) {
  renderWithQuery(
    <RuleFilterFields
      bucket="releases"
      prefix="ror/"
      include={[]}
      exclude={['*.tmp', 'builds/**']}
      onChange={onChange}
      yamlBase="storage.replication.rules[]"
      verb="replicate"
      excludeHelp="One glob per line."
      inputRadius={{ borderRadius: 6 }}
    />,
  );
  return onChange;
}

test('each glob shows its matches; a glob without the prefix offers the prefixed one', async () => {
  const onChange = section();
  const user = userEvent.setup();
  const list = await screen.findByRole('list', { name: 'exclude glob matches' });
  expect(within(list).getByText('matches 1')).toBeInTheDocument();
  expect(within(list).getByText('matches nothing')).toBeInTheDocument();
  expect(screen.getByRole('status', { name: 'Filter preview summary' })).toHaveTextContent(
    'Of the 3 objects under releases/ror/, 1 replicate and 1 is excluded.',
  );
  await user.click(within(list).getByRole('button', { name: 'Use ror/builds/**' }));
  expect(onChange).toHaveBeenCalledWith({ exclude_globs: ['*.tmp', 'ror/builds/**'] });
  expect(http.callsTo('POST', PREVIEW)[0].body).toMatchObject({
    bucket: 'releases',
    prefix: 'ror/',
    include_globs: [],
    exclude_globs: ['*.tmp', 'builds/**'],
  });
});

test('a group with more objects than the listed keys says how many it lists', async () => {
  http.on('POST', PREVIEW, json({ ...sample, sampled: 93_723, included: 12_815, keys: [{ key: 'ror/a-pre1.zip', verdict: 'included' }] }));
  section();
  const user = userEvent.setup();
  await screen.findByRole('list', { name: 'exclude glob matches' });
  await user.click(screen.getByRole('button', { name: /Preview objects/ }));
  const dialog = await screen.findByRole('dialog');
  expect(await within(dialog).findByText('Showing the first 1 of 12,815 objects in this group.')).toBeInTheDocument();
});

test('the preview window lists the keys by verdict and tests a typed key', async () => {
  section();
  const user = userEvent.setup();
  await screen.findByRole('list', { name: 'exclude glob matches' });
  await user.click(screen.getByRole('button', { name: /Preview objects/ }));
  const dialog = await screen.findByRole('dialog');
  const keys = within(dialog).getByRole('list', { name: 'Sampled objects' });
  expect(within(keys).getByText('a.zip')).toBeInTheDocument();
  expect(within(keys).queryByText('b.tmp')).toBeNull();
  await user.click(within(dialog).getByText('Excluded · 1'));
  expect(within(keys).getByText('b.tmp')).toBeInTheDocument();
  await user.type(within(dialog).getByLabelText('Test a key'), 'ror/new/c.tmp');
  await waitFor(() =>
    expect(within(dialog).getByRole('status', { name: 'Key verdict' })).toHaveTextContent('Excluded by *.tmp'),
  );
});
