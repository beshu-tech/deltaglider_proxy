import assert from 'node:assert/strict';
import { test } from 'vitest';
import { logLevelRadio, findMatchingPreset } from '../components/admin/logLevelPresets';

const INFO = 'deltaglider_proxy=info,tower_http=info';

test('preset matching ignores directive order and whitespace', () => {
  assert.equal(findMatchingPreset('tower_http=info, deltaglider_proxy=info'), INFO);
  assert.equal(findMatchingPreset('info,deltaglider_proxy::api=trace'), null);
});

test('log level radio reflects preset vs custom state', () => {
  // A preset value shows its preset, a non-preset value shows Custom.
  assert.deepEqual(logLevelRadio(INFO, false), { value: INFO, custom: false });
  assert.deepEqual(logLevelRadio('info,x=trace', false), { value: '__custom__', custom: true });
  // Clicking Custom on a preset value opens the editor without changing it.
  assert.deepEqual(logLevelRadio(INFO, true), { value: '__custom__', custom: true });
  // THE bug: after Discard the value is the preset again and the pick is
  // cleared, so the radio must show the preset, not a stuck "Custom".
  assert.deepEqual(logLevelRadio(INFO, false), { value: INFO, custom: false });
  // The section API omits a default-valued log_level: absent means the
  // server default (Debug), not "nothing selected".
  assert.deepEqual(logLevelRadio(undefined, false), {
    value: 'deltaglider_proxy=debug,tower_http=debug',
    custom: false,
  });
});
