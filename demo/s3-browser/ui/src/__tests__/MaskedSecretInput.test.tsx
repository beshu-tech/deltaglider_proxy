/**
 * MaskedSecretInput: what an empty field means is set by `mode`, never
 * inferred. A wrong answer clears or fails to rotate a credential.
 *
 * The harness mirrors the real consumers (WebhookDeliveryPanel header value,
 * SlackConnectorCard bot token): form state holds the raw value plus a
 * `masked` flag, and typing folds `{ value, masked: false }` back in.
 */
import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { useState } from 'react';
import { describe, expect, test, vi } from 'vitest';
import MaskedSecretInput from '../components/MaskedSecretInput';

const SENTINEL = '__redacted__';

interface SecretState {
  value: string;
  masked: boolean;
}

function SentinelHarness({ onState }: { onState: (s: SecretState) => void }) {
  const [state, setState] = useState<SecretState>({ value: SENTINEL, masked: true });
  return (
    <>
      <MaskedSecretInput
        mode="sentinel"
        value={state.value}
        masked={state.masked}
        onChange={(value) => {
          const next = { value, masked: false };
          setState(next);
          onState(next);
        }}
      />
      {/* What the consumer would put in the PUT body. */}
      <output data-testid="payload">{state.value}</output>
    </>
  );
}

function BlankKeepsHarness({ initial, onChangeSpy }: { initial: string; onChangeSpy: (v: string) => void }) {
  const [value, setValue] = useState(initial);
  return (
    <MaskedSecretInput
      mode="blank-keeps"
      value={value}
      onChange={(v) => {
        setValue(v);
        onChangeSpy(v);
      }}
    />
  );
}

/** Input.Password renders a plain <input type="password">, which has no role. */
function secretField(): HTMLInputElement {
  const el = document.querySelector('input');
  if (!el) throw new Error('no input rendered');
  return el;
}

describe('mode="sentinel"', () => {
  test('a masked value renders EMPTY with the "unchanged" placeholder; the sentinel never reaches the DOM', () => {
    render(<SentinelHarness onState={vi.fn()} />);
    const input = secretField();
    expect(input).toHaveValue('');
    expect(input).toHaveAttribute('placeholder', expect.stringContaining('unchanged'));
    expect(input.outerHTML).not.toContain(SENTINEL);
  });

  test('no edit emits nothing, so the sentinel passes through to the payload untouched', async () => {
    const onState = vi.fn();
    render(<SentinelHarness onState={onState} />);
    const user = userEvent.setup();
    // Focus and leave without typing: an operator who only looked at the field.
    await user.click(secretField());
    await user.tab();
    expect(onState).not.toHaveBeenCalled();
    expect(screen.getByTestId('payload')).toHaveTextContent(SENTINEL);
  });

  test('typing emits the literal input, unmasks, and shows the live value', async () => {
    const onState = vi.fn();
    render(<SentinelHarness onState={onState} />);
    const user = userEvent.setup();
    await user.type(secretField(), 'xoxb-1');
    expect(onState).toHaveBeenLastCalledWith({ value: 'xoxb-1', masked: false });
    expect(secretField()).toHaveValue('xoxb-1');
    expect(screen.getByTestId('payload')).toHaveTextContent('xoxb-1');
    // The typed text starts from empty: the sentinel is never a prefix.
    expect(onState).toHaveBeenNthCalledWith(1, { value: 'x', masked: false });
  });

  test('reveal renders a plain text input (webhook header value), same masking', () => {
    render(<MaskedSecretInput mode="sentinel" reveal value={SENTINEL} masked onChange={vi.fn()} />);
    const input = screen.getByRole('textbox');
    expect(input).toHaveValue('');
  });
});

describe('mode="blank-keeps"', () => {
  test('shows the value verbatim with the "leave blank to keep" placeholder', () => {
    render(<MaskedSecretInput mode="blank-keeps" value="" onChange={vi.fn()} />);
    const input = secretField();
    expect(input).toHaveValue('');
    expect(input).toHaveAttribute('placeholder', '(leave blank to keep existing)');
  });

  test('a non-blank input rotates: the typed secret is emitted', async () => {
    const spy = vi.fn();
    render(<BlankKeepsHarness initial="" onChangeSpy={spy} />);
    const user = userEvent.setup();
    await user.type(secretField(), 'n3w');
    expect(spy).toHaveBeenLastCalledWith('n3w');
    expect(secretField()).toHaveValue('n3w');
  });

  test('clearing back to blank emits "" (= keep existing), not a stale value', async () => {
    const spy = vi.fn();
    render(<BlankKeepsHarness initial="ab" onChangeSpy={spy} />);
    const user = userEvent.setup();
    await user.clear(secretField());
    expect(spy).toHaveBeenLastCalledWith('');
    expect(secretField()).toHaveValue('');
  });
});

test('every mode defaults autoComplete to new-password (no password-manager fill)', () => {
  const { unmount } = render(<MaskedSecretInput mode="sentinel" value={SENTINEL} masked onChange={vi.fn()} />);
  expect(secretField()).toHaveAttribute('autocomplete', 'new-password');
  unmount();
  render(<MaskedSecretInput mode="blank-keeps" value="" onChange={vi.fn()} />);
  expect(secretField()).toHaveAttribute('autocomplete', 'new-password');
});
