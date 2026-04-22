/**
 * Client-side Zod schema for an operator-authored `AdmissionBlock`.
 *
 * Mirrors the server's `AdmissionBlockSpec` validator (see
 * `src/admission/spec.rs`). The client catches the cheap typos
 * (format, length, charset) and the server stays the final gate —
 * the client never ships a block that would surface server-side with
 * a warning-only response.
 *
 * ## Server rules we duplicate here
 *
 *   * `name`: 1-128 chars, `[A-Za-z0-9_:.-]`, NOT starting with the
 *     reserved `public-prefix:` prefix (which is reserved for
 *     synthesised blocks).
 *   * `match.method[]`: uppercase HTTP verbs.
 *   * `match.source_ip` and `match.source_ip_list` are mutually
 *     exclusive (only one of the two forms per block).
 *   * `match.source_ip_list` cap: 4096 entries.
 *   * `match.path_glob`: must compile (server checks this with the
 *     `glob` crate; we accept any non-empty string and trust the
 *     server's compile check to catch invalid globs).
 *   * `action.reject.status`: 400-599.
 *   * `action.reject.message`: optional, up to 4096 chars.
 *
 * The schema type is what `react-hook-form` binds to. The submit
 * path converts this back to the server's `AdmissionBlock` shape
 * (which matches exactly).
 */
import { z } from 'zod';

export const METHODS = [
  'GET',
  'HEAD',
  'PUT',
  'POST',
  'DELETE',
  'PATCH',
  'OPTIONS',
] as const;

const RESERVED_NAME_PREFIX = 'public-prefix:';
const NAME_PATTERN = /^[A-Za-z0-9_:.-]+$/;

const admissionMatchSchema = z
  .object({
    method: z.array(z.enum(METHODS)).optional(),
    source_ip: z
      .string()
      .trim()
      .min(1, 'IP must not be empty')
      .max(64)
      .optional(),
    source_ip_list: z
      .array(z.string().trim().min(1, 'entry must not be empty').max(64))
      .max(4096, 'source_ip_list accepts at most 4096 entries')
      .optional(),
    bucket: z
      .string()
      .trim()
      .min(1, 'bucket name must not be empty')
      .max(128)
      .optional(),
    path_glob: z
      .string()
      .trim()
      .min(1, 'path_glob must not be empty')
      .max(1024)
      .optional(),
    authenticated: z.boolean().optional(),
    config_flag: z
      .string()
      .trim()
      .min(1, 'config_flag must not be empty')
      .max(128)
      .optional(),
  })
  .refine(
    (m) => !(m.source_ip && m.source_ip_list && m.source_ip_list.length > 0),
    {
      message:
        'source_ip and source_ip_list cannot both be set; pick one form',
      path: ['source_ip_list'],
    }
  );

const admissionRejectSchema = z.object({
  type: z.literal('reject'),
  status: z
    .number()
    .int()
    .min(400, 'reject status must be 4xx or 5xx')
    .max(599, 'reject status must be 4xx or 5xx'),
  message: z.string().max(4096).optional(),
});

/**
 * Matching the server's `Action` enum. Simple-string actions
 * (`allow-anonymous`, `deny`, `continue`) stay strings; reject is
 * the only structured variant.
 */
const admissionActionSchema = z.union([
  z.literal('allow-anonymous'),
  z.literal('deny'),
  z.literal('continue'),
  admissionRejectSchema,
]);

export const admissionBlockSchema = z.object({
  name: z
    .string()
    .trim()
    .min(1, 'name is required')
    .max(128, 'name must be at most 128 characters')
    .regex(
      NAME_PATTERN,
      'name may only contain letters, digits, and the characters _ : . -'
    )
    .refine((n) => !n.startsWith(RESERVED_NAME_PREFIX), {
      message:
        'names starting with "public-prefix:" are reserved for synthesised blocks',
    }),
  match: admissionMatchSchema,
  action: admissionActionSchema,
});

export type AdmissionBlockForm = z.infer<typeof admissionBlockSchema>;

/**
 * Narrow an `AdmissionBlock['action']` to its discriminant kind.
 *
 * Simple-string actions (`allow-anonymous`, `deny`, `continue`)
 * stay strings; the only structured variant is Reject. Centralised
 * here (rather than in each component that renders an action badge
 * / radio) so UIs and schemas agree on the kind set. Accepts
 * `unknown` to tolerate round-tripped payloads whose shape is
 * assumed but not proved — returns the narrow kind when the input
 * matches, falls through to `reject` otherwise (the only object-
 * shaped variant).
 */
export function actionKind(
  action: unknown
): 'allow-anonymous' | 'deny' | 'reject' | 'continue' {
  if (action === 'allow-anonymous' || action === 'deny' || action === 'continue') {
    return action;
  }
  return 'reject';
}
