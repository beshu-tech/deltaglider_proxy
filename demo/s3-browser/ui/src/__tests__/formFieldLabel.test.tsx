// FormField's label names the control it wraps (review4 frontend-3): a
// screen reader announces "Listen address", not "edit text".
import { screen } from '@testing-library/react';
import { Input, Switch } from 'antd';
import { afterEach, beforeEach, expect, test, vi } from 'vitest';
import FormField from '../components/FormField';
import MaskedSecretInput from '../components/MaskedSecretInput';
import { json, mockFetch } from '../test/fetchMock';
import { renderWithQuery } from '../test/render';

beforeEach(() => {
  mockFetch().on('GET', '/_/api/admin/config', json({ env_overrides: [] }));
});
afterEach(() => vi.unstubAllGlobals());

test('an Input inside FormField is named by the label', () => {
  renderWithQuery(
    <FormField label="Listen address" yamlPath="advanced.listen_addr" helpText="host:port">
      <Input value="0.0.0.0:9000" onChange={() => {}} />
    </FormField>,
  );
  const box = screen.getByRole('textbox');
  expect(box).toHaveAccessibleName('Listen address');
});

test('a Switch inside FormField is named by the label', () => {
  renderWithQuery(
    <FormField label="Enable TLS" yamlPath="advanced.tls.enabled">
      <Switch checked={false} onChange={() => {}} />
    </FormField>,
  );
  expect(screen.getByRole('switch')).toHaveAccessibleName('Enable TLS');
});

test('a MaskedSecretInput inside FormField is named by the label', () => {
  const { container } = renderWithQuery(
    <FormField label="Secret access key" yamlPath="storage.backend.secret_access_key">
      <MaskedSecretInput mode="new" value="" onChange={() => {}} />
    </FormField>,
  );
  const input = container.querySelector('input[type="password"]');
  expect(input).not.toBeNull();
  expect(input).toHaveAccessibleName('Secret access key');
});

test("a child's own aria-label wins over the FormField label", () => {
  renderWithQuery(
    <FormField label="Enable TLS" yamlPath="advanced.tls.enabled">
      <Switch aria-label="Serve HTTPS" checked={false} onChange={() => {}} />
    </FormField>,
  );
  expect(screen.getByRole('switch')).toHaveAccessibleName('Serve HTTPS');
});
