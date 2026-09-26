/**
 * Pure-ish credential generators for the IAM UserForm "generate random
 * key/secret" buttons. The randomness source is injectable so the format
 * invariants (prefix, length, alphabet) can be exercised deterministically in
 * the unit test without touching real entropy.
 */

const ID_ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789';
const SECRET_ALPHABET = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/';

const ID_PREFIX = 'AK';
const ID_BODY_LENGTH = 18;
const SECRET_LENGTH = 40;

/** Fill `buf` with random bytes. Defaults to the Web Crypto CSPRNG. */
type RandomFill = (buf: Uint8Array<ArrayBuffer>) => void;

const defaultFill: RandomFill = (buf) => crypto.getRandomValues(buf);

/** Random string of `length` chars drawn from `alphabet` via `fill`. */
function secureRandom(alphabet: string, length: number, fill: RandomFill): string {
  const buf = new Uint8Array(length);
  fill(buf);
  return Array.from(buf, (b) => alphabet[b % alphabet.length]).join('');
}

/** Generate an access key id: `AK` + 18 uppercase-alnum chars. */
export function generateId(fill: RandomFill = defaultFill): string {
  return ID_PREFIX + secureRandom(ID_ALPHABET, ID_BODY_LENGTH, fill);
}

/** Generate a 40-char base64-alphabet secret access key. */
export function generateSecret(fill: RandomFill = defaultFill): string {
  return secureRandom(SECRET_ALPHABET, SECRET_LENGTH, fill);
}
