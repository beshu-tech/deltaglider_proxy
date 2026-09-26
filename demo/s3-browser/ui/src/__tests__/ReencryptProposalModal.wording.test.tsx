/**
 * Explore finding 19: the re-encrypt proposal said rewritten objects get a
 * new Last-Modified, but the job keeps each object's created-at (served as
 * LastModified), so sync tools do not re-download them.
 */
import { screen } from '@testing-library/react';
import { expect, test } from 'vitest';
import ReencryptProposalModal from '../components/ReencryptProposalModal';
import { renderWithQuery } from '../test/render';

test('the proposal says rewritten objects keep their Last-Modified', async () => {
  renderWithQuery(
    <ReencryptProposalModal open transition="rotate" backendName="local-disk" buckets={['releases']} onClose={() => {}} />,
  );
  expect(await screen.findByText(/keep their Last-Modified/)).toBeInTheDocument();
  expect(screen.queryByText(/get a new Last-Modified/)).toBeNull();
});
