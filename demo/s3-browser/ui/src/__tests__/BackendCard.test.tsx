/**
 * The backend card: name, status pills and kind in the header; the facts
 * as a labelled list OUTSIDE the header (they used to share a row with the
 * buttons, so a long endpoint wrapped in a narrow column beside them); the
 * "⋯" menu in its own slot.
 */
import { screen, within } from '@testing-library/react';
import { expect, test } from 'vitest';
import BackendCard from '../components/BackendCard';
import { backendFacts } from '../backendSummary';
import { renderWithQuery } from '../test/render';

test('header, facts list and menu', () => {
  renderWithQuery(
    <BackendCard
      name="hetzner-fsn1"
      filesystem={false}
      kind="S3-compatible"
      highlighted
      pills={[{ label: 'Default', color: '#0f766e' }, { label: 'Connected', color: '#047857', dot: true }]}
      facts={[
        ...backendFacts({ backend_type: 's3', path: null, endpoint: 'https://fsn1.your-objectstorage.com', region: 'fsn1', access_key_id: 'AKHETZNER' }),
        { label: 'Buckets', value: '3 routed here', mono: false },
      ]}
      actions={<button type="button">Test connection</button>}
      menu={<button type="button">More actions</button>}
    >
      <p>encryption editor</p>
    </BackendCard>,
  );
  const card = screen.getByRole('article', { name: 'Backend hetzner-fsn1' });
  const header = within(card).getByRole('banner');
  expect(within(header).getByRole('heading', { name: 'hetzner-fsn1' })).toBeInTheDocument();
  expect(header).toHaveTextContent('Default');
  expect(header).toHaveTextContent('Connected');
  expect(header).toHaveTextContent('S3-compatible');
  expect(within(header).getByRole('button', { name: 'Test connection' })).toBeInTheDocument();
  expect(within(header).getByRole('button', { name: 'More actions' })).toBeInTheDocument();

  const terms = within(card).getAllByRole('term').map((t) => t.textContent);
  const values = within(card).getAllByRole('definition').map((d) => d.textContent);
  expect(terms).toEqual(['Endpoint', 'Region', 'Access key', 'Buckets']);
  expect(values).toEqual(['https://fsn1.your-objectstorage.com', 'fsn1', 'AKHETZNER', '3 routed here']);
  expect(header).not.toHaveTextContent('fsn1.your-objectstorage.com');
  expect(card).toHaveTextContent('encryption editor');
});
