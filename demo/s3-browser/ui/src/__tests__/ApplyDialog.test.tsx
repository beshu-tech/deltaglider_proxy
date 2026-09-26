/**
 * ApplyDialog: the plan → diff → apply confirmation. It renders what the
 * server said (diff, new vs existing warnings, restart) and gates Apply on
 * `ok`. The parent runs the real PUT.
 */
import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, test, vi } from 'vitest';
import type { SectionApplyResponse } from '../adminApi';
import ApplyDialog from '../components/ApplyDialog';

function renderDialog(response: SectionApplyResponse | null, extra: { loading?: boolean; open?: boolean } = {}) {
  const onApply = vi.fn();
  const onCancel = vi.fn();
  render(
    <ApplyDialog
      open={extra.open ?? true}
      section="advanced"
      response={response}
      onApply={onApply}
      onCancel={onCancel}
      loading={extra.loading}
      summary={<p>Human summary</p>}
    />,
  );
  return { onApply, onCancel };
}

const applyButton = () => screen.getByRole('button', { name: 'Apply and persist changes' });

test('no response renders nothing', () => {
  renderDialog(null);
  expect(screen.queryByRole('dialog')).not.toBeInTheDocument();
});

describe('ok response', () => {
  const ok: SectionApplyResponse = {
    ok: true,
    diff: {
      advanced: {
        cache_size_mb: { before: 100, after: 512 },
        log_level: { before: 'info', after: 'debug' },
      },
      // Another section's diff must not leak into this section's dialog.
      storage: { 'buckets.releases': { before: null, after: {} } },
    },
  };

  test('renders the section diff as before/after rows, and only this section', () => {
    renderDialog(ok);
    const dialog = screen.getByRole('dialog');
    expect(within(dialog).getByText('advanced')).toBeInTheDocument();
    expect(within(dialog).getByText('Changes (2)')).toBeInTheDocument();
    expect(within(dialog).getByText('cache_size_mb')).toBeInTheDocument();
    expect(within(dialog).getByText(/-\s100/)).toBeInTheDocument();
    expect(within(dialog).getByText(/\+\s512/)).toBeInTheDocument();
    // Strings are JSON-quoted.
    expect(within(dialog).getByText(/\+\s"debug"/)).toBeInTheDocument();
    expect(within(dialog).queryByText('buckets.releases')).not.toBeInTheDocument();
    expect(within(dialog).getByText('Human summary')).toBeInTheDocument();
  });

  test('Apply calls onApply; Cancel calls onCancel', async () => {
    const { onApply, onCancel } = renderDialog(ok);
    const user = userEvent.setup();
    expect(applyButton()).toBeEnabled();
    await user.click(applyButton());
    expect(onApply).toHaveBeenCalledTimes(1);
    await user.click(screen.getByRole('button', { name: 'Cancel' }));
    expect(onCancel).toHaveBeenCalledTimes(1);
  });

  test('an empty diff is a no-op apply that is still allowed', () => {
    renderDialog({ ok: true, diff: {} });
    expect(screen.getByText('Changes (0)')).toBeInTheDocument();
    expect(screen.getByText(/this apply would be a no-op/)).toBeInTheDocument();
    expect(applyButton()).toBeEnabled();
  });

  test('loading disables both buttons (PUT in flight)', () => {
    renderDialog(ok, { loading: true });
    expect(applyButton()).toBeDisabled();
    expect(screen.getByRole('button', { name: 'Cancel' })).toBeDisabled();
  });
});

test('ok:false shows the validation error and disables Apply', async () => {
  const { onApply } = renderDialog({ ok: false, error: 'cache_size_mb must be > 0' });
  expect(screen.getByText('Validation failed')).toBeInTheDocument();
  expect(screen.getByText('cache_size_mb must be > 0')).toBeInTheDocument();
  expect(applyButton()).toBeDisabled();
  await userEvent.setup().click(applyButton());
  expect(onApply).not.toHaveBeenCalled();
});

test('new warnings show open; existing warnings fold into a closed <details>', () => {
  renderDialog({
    ok: true,
    diff: {},
    warnings: ['bucket "releases" has no backend'],
    existing_warnings: ['old warning one', 'old warning two'],
  });
  expect(screen.getByText('1 warning from this change')).toBeInTheDocument();
  expect(screen.getByText('bucket "releases" has no backend')).toBeInTheDocument();
  const existing = screen.getByTestId('apply-dialog-existing-warnings');
  expect(existing.tagName).toBe('DETAILS');
  expect(existing).not.toHaveAttribute('open');
  expect(within(existing).getByText('2 warnings that already existed before this change')).toBeInTheDocument();
  expect(within(existing).getByText('old warning one')).toBeInTheDocument();
  // Warnings never block Apply.
  expect(applyButton()).toBeEnabled();
});

test('a doc URL inside a warning renders as a link', () => {
  renderDialog({
    ok: true,
    diff: {},
    warnings: ['see https://deltaglider.com/docs/how-to/backend-capability-validation for details'],
  });
  expect(screen.getByRole('link')).toBeInTheDocument();
});

test('requires_restart shows the restart banner; absent means no banner', () => {
  renderDialog({ ok: true, diff: {}, requires_restart: true });
  expect(screen.getByText('Restart required')).toBeInTheDocument();
});

test('no restart banner without requires_restart', () => {
  renderDialog({ ok: true, diff: {} });
  expect(screen.queryByText('Restart required')).not.toBeInTheDocument();
});
