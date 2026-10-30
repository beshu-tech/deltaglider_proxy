/**
 * UX harvest B68: an unknown docs address (/_/docs/configuration) showed the
 * docs landing page with no word that the page does not exist.
 */
import { screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { beforeAll, expect, test, vi } from 'vitest';
import DocsPage from '../components/DocsPage';
import { NavigationContext } from '../NavigationContext';
import { buildDocsBundle } from '../docsBundle';
import { qk } from '../queries/keys';
import { buildViewUrl } from '../urlState';
import { renderWithQuery, testQueryClient } from '../test/render';

beforeAll(() => {
  // jsdom has no element scrolling; the page scrolls its content pane.
  Element.prototype.scrollTo = () => {};
});

const BUNDLE = buildDocsBundle({
  manifest: {
    groups: [{ id: 'Start here', tagline: 'Lessons.' }],
    docs: [
      { path: 'readme', group: 'Start here', order: 0 },
      { path: 'faq', group: 'Start here', order: 1 },
    ],
  },
  docs: [
    { path: 'readme', content: '# DeltaGlider Proxy\ntext' },
    { path: 'faq', content: '# Common questions\nanswers' },
  ],
});

function renderDocs(docId?: string) {
  const navigate = vi.fn();
  const client = testQueryClient();
  client.setQueryData(qk.docs(), BUNDLE);
  renderWithQuery(
    <NavigationContext.Provider value={{ navigate, subPath: docId ?? '' }}>
      <DocsPage docId={docId} />
    </NavigationContext.Provider>,
    { client },
  );
  return navigate;
}

test('an unknown docs address says that no page exists and links to the index', async () => {
  const navigate = renderDocs('configuration');
  expect(screen.getByText('No page at this address')).toBeInTheDocument();
  expect(screen.getByText('/_/docs/configuration')).toBeInTheDocument();
  await userEvent.click(screen.getByRole('button', { name: 'Go to the docs index' }));
  expect(navigate).toHaveBeenCalledWith(buildViewUrl('docs'));
});

test('a known docs address shows the page, not the notice', () => {
  renderDocs('faq');
  expect(screen.queryByText('No page at this address')).toBeNull();
  expect(screen.getByRole('heading', { name: 'Common questions' })).toBeInTheDocument();
});

test('the docs root shows the landing page, not the notice', () => {
  renderDocs();
  expect(screen.queryByText('No page at this address')).toBeNull();
});
