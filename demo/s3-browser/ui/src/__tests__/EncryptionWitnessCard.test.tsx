/**
 * Issue #116 P3: the System page's encryption witness card. It shows the
 * verdict of this instance (agree / mismatch / unknown / unavailable), each
 * mismatch with this instance's form against the published one, and a
 * publish per mismatch and for all. A plaintext publish asks for the
 * downgrade confirmation and sends it as a header. A single instance
 * (`null`) shows nothing.
 */
import { screen, waitFor, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import EncryptionWitnessCard from '../components/EncryptionWitnessCard';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

const WITNESS = '/_/api/admin/encryption-witness';
const PUBLISH = '/_/api/admin/encryption-witness/publish';
const BACKEND = {
  name: 'hetzner-fsn1', backend_type: 's3', path: null, endpoint: null, region: null, force_path_style: null,
  has_credentials: true, encryption: { mode: 'none', has_key: false, shim_active: false },
};
const AES = { mode: 'aes256-gcm-proxy', key_id: 'releases-2026-10', kcv: '0011223344556677' };
const PLAIN = { mode: 'none' };

let http: ReturnType<typeof mockFetch>;
beforeEach(() => {
  http = mockFetch();
  http.on('GET', '/_/api/admin/config', json({
    env_overrides: [], backends: [BACKEND], default_backend: 'hetzner-fsn1',
    bucket_policies: { releases: { backend: 'hetzner-fsn1' }, 'db-archive': { backend: 'hetzner-fsn1' } },
  }));
  http.on('POST', PUBLISH, json({ published: [], status: 'agree' }));
});
afterEach(() => vi.unstubAllGlobals());

const mismatch = (bucket: string, local: object, published: object) => ({
  backend: 'hetzner-fsn1', bucket, local, published, set_by: 'dgp-0',
});

test('a single instance shows no card', async () => {
  http.on('GET', WITNESS, json(null));
  const { container } = renderWithQuery(<EncryptionWitnessCard />);
  await waitFor(() => expect(http.callsTo('GET', WITNESS)).toHaveLength(1));
  // Let the answer render.
  await new Promise((r) => setTimeout(r, 0));
  expect(container).toBeEmptyDOMElement();
});

test('agreement says so and offers no publish', async () => {
  http.on('GET', WITNESS, json({ status: 'agree' }));
  renderWithQuery(<EncryptionWitnessCard />);
  expect(await screen.findByTestId('witness-status')).toHaveTextContent('Agree');
  expect(screen.queryByRole('button', { name: /Publish/ })).toBeNull();
});

test('a mismatch lists the storage, both forms and who published', async () => {
  http.on('GET', WITNESS, json({
    status: 'mismatch',
    mismatches: [mismatch('releases', AES, PLAIN), mismatch('*', PLAIN, AES)],
  }));
  renderWithQuery(<EncryptionWitnessCard />);
  expect(await screen.findByTestId('witness-status')).toHaveTextContent('Mismatch');
  const rows = screen.getAllByRole('listitem');
  expect(rows[0]).toHaveTextContent('releases on backend hetzner-fsn1');
  expect(rows[0]).toHaveTextContent('This instance: aes256-gcm-proxy · key releases-2026-10 (check 0011223344556677)');
  expect(rows[0]).toHaveTextContent('Published: none (plaintext) (set by dgp-0)');
  expect(rows[1]).toHaveTextContent('All buckets without their own setting on backend hetzner-fsn1');
  expect(screen.getByRole('button', { name: 'Publish all' })).toBeInTheDocument();
});

test('publishing an encrypted form sends the bucket and no confirmation', async () => {
  http.on('GET', WITNESS, json({ status: 'mismatch', mismatches: [mismatch('releases', AES, PLAIN)] }));
  renderWithQuery(<EncryptionWitnessCard />);
  const user = userEvent.setup();
  const button = await screen.findByRole('button', { name: /Publish this instance's setting for releases/ });
  await waitFor(() => expect(button).toBeEnabled());
  await user.click(button);
  await waitFor(() => expect(http.callsTo('POST', PUBLISH)).toHaveLength(1));
  const req = http.callsTo('POST', PUBLISH)[0];
  expect(req.body).toEqual({ storages: ['releases'] });
  expect(req.headers['x-dgp-confirm-encryption-downgrade']).toBeUndefined();
  // The verdict is read again after the publish.
  await waitFor(() => expect(http.callsTo('GET', WITNESS).length).toBeGreaterThan(1));
});

test('publishing plaintext asks first; cancel sends nothing, confirm sends the header', async () => {
  http.on('GET', WITNESS, json({ status: 'mismatch', mismatches: [mismatch('*', PLAIN, AES)] }));
  renderWithQuery(<EncryptionWitnessCard />);
  const user = userEvent.setup();
  const button = await screen.findByRole('button', { name: /Publish this instance's setting for All buckets/ });
  await waitFor(() => expect(button).toBeEnabled());

  await user.click(button);
  let dialog = await screen.findByRole('dialog');
  expect(dialog).toHaveTextContent('Publish plaintext for the cluster?');
  expect(dialog).toHaveTextContent('route:default');
  await user.click(within(dialog).getByRole('button', { name: 'Cancel' }));
  await waitFor(() => expect(screen.queryByRole('dialog')).toBeNull());
  expect(http.callsTo('POST', PUBLISH)).toHaveLength(0);

  await user.click(button);
  dialog = await screen.findByRole('dialog');
  await user.click(within(dialog).getByRole('button', { name: 'Publish plaintext' }));
  await waitFor(() => expect(http.callsTo('POST', PUBLISH)).toHaveLength(1));
  const req = http.callsTo('POST', PUBLISH)[0];
  expect(req.body).toEqual({ storages: ['route:default'] });
  expect(req.headers['x-dgp-confirm-encryption-downgrade']).toBe('route:default');
});

test('publish all sends every subject, with the plaintext ones confirmed', async () => {
  http.on('GET', WITNESS, json({
    status: 'mismatch',
    mismatches: [mismatch('releases', AES, PLAIN), mismatch('db-archive', PLAIN, AES)],
  }));
  renderWithQuery(<EncryptionWitnessCard />);
  const user = userEvent.setup();
  const all = await screen.findByRole('button', { name: 'Publish all' });
  await waitFor(() => expect(all).toBeEnabled());
  await user.click(all);
  const dialog = await screen.findByRole('dialog');
  await user.click(within(dialog).getByRole('button', { name: 'Publish plaintext' }));
  await waitFor(() => expect(http.callsTo('POST', PUBLISH)).toHaveLength(1));
  const req = http.callsTo('POST', PUBLISH)[0];
  expect(req.body).toEqual({ storages: ['releases', 'db-archive'] });
  expect(req.headers['x-dgp-confirm-encryption-downgrade']).toBe('db-archive');
});

test('a refused publish shows the server message', async () => {
  http.on('GET', WITNESS, json({ status: 'mismatch', mismatches: [mismatch('releases', AES, PLAIN)] }));
  http.on('POST', PUBLISH, json({ error: 'the sync bucket is unreachable' }, 503));
  renderWithQuery(<EncryptionWitnessCard />);
  const user = userEvent.setup();
  const button = await screen.findByRole('button', { name: /Publish this instance's setting for releases/ });
  await waitFor(() => expect(button).toBeEnabled());
  await user.click(button);
  expect(await screen.findByText(/the sync bucket is unreachable/)).toBeInTheDocument();
});

test('unknown and unavailable name the state and show the sync error', async () => {
  http.on('GET', WITNESS, json({ status: 'unknown', error: 'GET _dgp/witness: connection refused' }));
  renderWithQuery(<EncryptionWitnessCard />);
  expect(await screen.findByTestId('witness-status')).toHaveTextContent('Unknown');
  expect(screen.getByText('GET _dgp/witness: connection refused')).toBeInTheDocument();
  expect(screen.getByText(/did not answer/)).toBeInTheDocument();
});

test('unavailable says that the witnessed buckets refuse writes', async () => {
  http.on('GET', WITNESS, json({ status: 'unavailable', error: 'no coordination client' }));
  renderWithQuery(<EncryptionWitnessCard />);
  expect(await screen.findByTestId('witness-status')).toHaveTextContent('Unavailable');
  expect(screen.getByText(/refuses writes to the buckets that the witness covers/)).toBeInTheDocument();
});
