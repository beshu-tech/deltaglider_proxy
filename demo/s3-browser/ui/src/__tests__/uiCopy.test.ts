/**
 * Help texts that state a server behaviour. Each case names the server rule
 * that it must match, so a copy change that contradicts the server fails here.
 */
import { readFileSync } from 'node:fs';
import { expect, test } from 'vitest';

const src = (rel: string) => readFileSync(new URL(`../${rel}`, import.meta.url), 'utf8');

// event_delivery.rs inactive_prune_floor: with delivery off, the outbox keeps
// an event until event-driven replication consumes it (keeps all while no
// replication cursor is active), and prunes it at once only with replication off.
test('Enable delivery help matches the outbox prune rule', () => {
  const panel = src('components/WebhookDeliveryPanel.tsx');
  expect(panel).not.toContain('events are not kept for later');
  expect(panel).toMatch(/Enable delivery[\s\S]{0,400}replication/);
});
