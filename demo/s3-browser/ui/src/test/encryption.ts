/**
 * Test fixtures for the server's per-bucket `effective_encryption`
 * (GET /api/admin/buckets, issue #116).
 */
import type { EffectiveEncryption } from '../adminApi';

/** A plaintext bucket that inherits from `hetzner-fsn1`, with overrides. */
export function eff(over: Partial<EffectiveEncryption> = {}): EffectiveEncryption {
  return {
    mode: 'none',
    source: 'backend',
    backend: 'hetzner-fsn1',
    has_key: false,
    encrypts: false,
    reencrypt: false,
    pinned: true,
    bucket_modes: ['none', 'aes256-gcm-proxy'],
    inherited: { mode: 'none', encrypts: false },
    ...over,
  };
}

/** A bucket with its own proxy-AES key on `hetzner-fsn1`. */
export function bucketKeyEff(over: Partial<EffectiveEncryption> = {}): EffectiveEncryption {
  return eff({
    mode: 'aes256-gcm-proxy',
    source: 'bucket',
    has_key: true,
    encrypts: true,
    reencrypt: true,
    key_id: 'db-archive-2026-10',
    ...over,
  });
}

/** A bucket that inherits proxy AES from its backend. */
export function inheritedAesEff(backend: string, keyId: string, over: Partial<EffectiveEncryption> = {}): EffectiveEncryption {
  return eff({
    backend,
    mode: 'aes256-gcm-proxy',
    has_key: true,
    encrypts: true,
    reencrypt: true,
    key_id: keyId,
    inherited: { mode: 'aes256-gcm-proxy', encrypts: true, key_id: keyId },
    ...over,
  });
}
