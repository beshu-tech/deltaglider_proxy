/**
 * AdmissionBlockEditorModal — the form surface for creating and
 * editing a single operator-authored admission block (§7.1 of the
 * admin UI revamp plan).
 *
 * ## Design
 *
 *   * Name field up top with inline Zod validation matching the
 *     server's rules (reserved `public-prefix:` prefix blocked with
 *     a link to the Storage tab instead).
 *   * Match predicates grouped into **four cards** so the operator
 *     sees them by concern, not as a flat list of 6 unrelated
 *     inputs:
 *       1. **Request** — method checkboxes (GET/HEAD/PUT/...).
 *       2. **Source IP** — mutually-exclusive single IP or IP list
 *          (with the list editor below).
 *       3. **Path & Bucket** — bucket name + path glob.
 *       4. **Auth state** — anonymous / authenticated / any.
 *   * Action radio group at the bottom with a conditional Reject
 *     sub-form (status + optional message). Destructive actions
 *     (deny, reject) get a muted reminder bar: "This will 403/5xx
 *     all matching requests."
 *
 * ## Validation
 *
 * react-hook-form + Zod via `admissionBlockSchema`. Submit is
 * blocked on any error. The parent owns persistence (calls
 * `putSection('admission', { blocks: [...] })` through the section
 * API).
 */
import { useEffect, useMemo } from 'react';
import { useForm, Controller } from 'react-hook-form';
import { zodResolver } from '@hookform/resolvers/zod';
import {
  Modal,
  Input,
  InputNumber,
  Checkbox,
  Radio,
  Select,
  Button,
  Alert,
  Space,
  Typography,
} from 'antd';
import type { AdmissionBlock } from '../adminApi';
import {
  actionKind,
  admissionBlockSchema,
  METHODS,
  type AdmissionBlockForm,
} from '../schemas/admissionSchema';
import { useColors } from '../ThemeContext';
import FormField from './FormField';

const { Text } = Typography;

interface Props {
  open: boolean;
  /** When editing, the existing block. When creating, `null`. */
  initial: AdmissionBlock | null;
  /**
   * Other blocks in the list, for duplicate-name validation. Passed
   * by reference so the modal can check names client-side without
   * the parent needing to pre-compute a set.
   */
  otherNames: string[];
  onCancel: () => void;
  onSave: (block: AdmissionBlock) => void;
}

/** Default empty-block form state. */
const EMPTY: AdmissionBlockForm = {
  name: '',
  match: {},
  action: 'deny',
};

export default function AdmissionBlockEditorModal({
  open,
  initial,
  otherNames,
  onCancel,
  onSave,
}: Props) {
  const { BORDER, BG_CARD, TEXT_MUTED } = useColors();
  const defaults = useMemo<AdmissionBlockForm>(() => {
    if (!initial) return EMPTY;
    return {
      name: initial.name,
      match: {
        method: initial.match.method as AdmissionBlockForm['match']['method'],
        source_ip: initial.match.source_ip,
        source_ip_list: initial.match.source_ip_list,
        bucket: initial.match.bucket,
        path_glob: initial.match.path_glob,
        authenticated: initial.match.authenticated,
        config_flag: initial.match.config_flag,
      },
      action: initial.action as AdmissionBlockForm['action'],
    };
  }, [initial]);

  const {
    control,
    handleSubmit,
    watch,
    setValue,
    setError,
    reset,
    formState: { errors, isSubmitting },
  } = useForm<AdmissionBlockForm>({
    resolver: zodResolver(admissionBlockSchema),
    defaultValues: defaults,
    mode: 'onChange',
  });

  // Re-seed when the modal reopens with a different initial block.
  useEffect(() => {
    if (open) reset(defaults);
  }, [open, defaults, reset]);

  // Watch the action to render the conditional Reject sub-form.
  const currentAction = watch('action');
  const kind = actionKind(currentAction);

  // Watch IP form so the two options are mutually exclusive — picking
  // one clears the other.
  const sourceIp = watch('match.source_ip');
  const sourceIpList = watch('match.source_ip_list');

  const onSubmit = (data: AdmissionBlockForm) => {
    // Duplicate-name check (case-insensitive, excluding the block
    // we're currently editing). Surface as an RHF field error so
    // the operator sees it inline under the Name input rather than
    // as an intrusive native dialog.
    const others = otherNames
      .filter((n) => !initial || n !== initial.name)
      .map((n) => n.toLowerCase());
    if (others.includes(data.name.toLowerCase())) {
      setError('name', {
        type: 'manual',
        message: `A block named "${data.name}" already exists.`,
      });
      return;
    }
    // Strip empty-array / empty-string / undefined fields from `match`
    // so the serialised YAML stays compact. The server tolerates
    // both shapes; the operator wouldn't want `method: []` in the
    // YAML when they meant "any method".
    const compact: AdmissionBlock['match'] = {};
    const m = data.match;
    if (m.method && m.method.length > 0) compact.method = m.method;
    if (m.source_ip && m.source_ip.trim())
      compact.source_ip = m.source_ip.trim();
    if (m.source_ip_list && m.source_ip_list.length > 0)
      compact.source_ip_list = m.source_ip_list;
    if (m.bucket && m.bucket.trim()) compact.bucket = m.bucket.trim();
    if (m.path_glob && m.path_glob.trim())
      compact.path_glob = m.path_glob.trim();
    if (m.authenticated !== undefined)
      compact.authenticated = m.authenticated;
    if (m.config_flag && m.config_flag.trim())
      compact.config_flag = m.config_flag.trim();

    onSave({ name: data.name, match: compact, action: data.action });
  };

  const cardStyle: React.CSSProperties = {
    border: `1px solid ${BORDER}`,
    borderRadius: 8,
    padding: 12,
    marginBottom: 12,
    background: BG_CARD,
  };
  const cardLabel: React.CSSProperties = {
    color: TEXT_MUTED,
    fontSize: 10,
    fontWeight: 700,
    letterSpacing: 0.5,
    textTransform: 'uppercase' as const,
    marginBottom: 8,
    fontFamily: 'var(--font-ui)',
  };

  return (
    <Modal
      open={open}
      onCancel={onCancel}
      title={initial ? `Edit admission block: ${initial.name}` : 'Add admission block'}
      width={720}
      destroyOnHidden
      footer={
        <Space>
          <Button onClick={onCancel}>Cancel</Button>
          <Button
            type="primary"
            onClick={handleSubmit(onSubmit)}
            loading={isSubmitting}
          >
            {initial ? 'Save' : 'Add block'}
          </Button>
        </Space>
      }
    >
      {/* Name */}
      <FormField
        label="Name"
        yamlPath="admission.blocks[].name"
        helpText="Unique identifier for this block. Letters, digits, and _ : . - only."
      >
        <Controller
          control={control}
          name="name"
          render={({ field }) => (
            <Input
              placeholder="e.g. deny-known-bad-ips"
              status={errors.name ? 'error' : undefined}
              value={field.value ?? ''}
              onChange={(e) => field.onChange(e.target.value)}
              onBlur={field.onBlur}
              ref={field.ref}
            />
          )}
        />
        {errors.name && (
          <Text type="danger" style={{ fontSize: 12 }}>
            {errors.name.message}
          </Text>
        )}
      </FormField>

      {/* Match: Request card */}
      <div style={cardStyle}>
        <div style={cardLabel}>Match — Request</div>
        <FormField
          label="HTTP methods"
          yamlPath="match.method"
          helpText="Leave empty to match any method."
        >
          <Controller
            control={control}
            name="match.method"
            render={({ field }) => (
              <Checkbox.Group
                options={METHODS.map((m) => ({ label: m, value: m }))}
                value={field.value as string[] | undefined}
                onChange={(v) =>
                  field.onChange((v as string[]) as AdmissionBlockForm['match']['method'])
                }
              />
            )}
          />
        </FormField>
      </div>

      {/* Match: Source IP card */}
      <div style={cardStyle}>
        <div style={cardLabel}>Match — Source IP</div>
        <Text type="secondary" style={{ fontSize: 12, display: 'block', marginBottom: 12 }}>
          Mutually exclusive: pick exactly one. Leave both empty to match any source.
        </Text>
        <FormField
          label="Single IP"
          yamlPath="match.source_ip"
          helpText="Exact IP address. Clears source_ip_list when set."
          examples={['203.0.113.5', '2001:db8::1']}
          onExampleClick={(v) => {
            setValue('match.source_ip', String(v));
            setValue('match.source_ip_list', undefined);
          }}
        >
          <Controller
            control={control}
            name="match.source_ip"
            render={({ field }) => (
              <Input
                placeholder="203.0.113.5"
                disabled={!!(sourceIpList && sourceIpList.length > 0)}
                status={errors.match?.source_ip ? 'error' : undefined}
                value={field.value ?? ''}
                onChange={(e) => field.onChange(e.target.value || undefined)}
                onBlur={field.onBlur}
                ref={field.ref}
              />
            )}
          />
        </FormField>
        <FormField
          label="IP / CIDR list"
          yamlPath="match.source_ip_list"
          helpText="One entry per line. Accepts IPs and CIDRs. Max 4096 entries."
          examples={['203.0.113.0/24', '2001:db8::/32']}
          onExampleClick={(v) => {
            const existing = sourceIpList || [];
            setValue('match.source_ip_list', [...existing, String(v)]);
            setValue('match.source_ip', undefined);
          }}
        >
          <Controller
            control={control}
            name="match.source_ip_list"
            render={({ field }) => (
              <Input.TextArea
                rows={4}
                placeholder={'203.0.113.0/24\n198.51.100.0/24'}
                disabled={!!(sourceIp && sourceIp.trim())}
                value={(field.value ?? []).join('\n')}
                onChange={(e) => {
                  const lines = e.target.value
                    .split('\n')
                    .map((l) => l.trim())
                    .filter((l) => l.length > 0);
                  field.onChange(lines.length > 0 ? lines : undefined);
                }}
              />
            )}
          />
          {errors.match?.source_ip_list && (
            <Text type="danger" style={{ fontSize: 12 }}>
              {errors.match.source_ip_list.message}
            </Text>
          )}
        </FormField>
      </div>

      {/* Match: Path & Bucket card */}
      <div style={cardStyle}>
        <div style={cardLabel}>Match — Path &amp; Bucket</div>
        <FormField
          label="Bucket"
          yamlPath="match.bucket"
          helpText="Target bucket (lowercased). Empty = any bucket."
        >
          <Controller
            control={control}
            name="match.bucket"
            render={({ field }) => (
              <Input
                placeholder="e.g. releases"
                value={field.value ?? ''}
                onChange={(e) => field.onChange(e.target.value || undefined)}
                onBlur={field.onBlur}
                ref={field.ref}
              />
            )}
          />
        </FormField>
        <FormField
          label="Path glob"
          yamlPath="match.path_glob"
          helpText="glob pattern on the object key. Uses the `glob` crate syntax."
          examples={['*.zip', 'builds/**', 'stable/*.tar.gz']}
          onExampleClick={(v) => setValue('match.path_glob', String(v))}
        >
          <Controller
            control={control}
            name="match.path_glob"
            render={({ field }) => (
              <Input
                placeholder="*.zip"
                value={field.value ?? ''}
                onChange={(e) => field.onChange(e.target.value || undefined)}
                onBlur={field.onBlur}
                ref={field.ref}
              />
            )}
          />
        </FormField>
      </div>

      {/* Match: Auth state card */}
      <div style={cardStyle}>
        <div style={cardLabel}>Match — Auth state</div>
        <FormField
          label="Authenticated?"
          yamlPath="match.authenticated"
          helpText="Pick one. Any = match regardless of auth state."
        >
          <Controller
            control={control}
            name="match.authenticated"
            render={({ field }) => (
              <Radio.Group
                value={
                  field.value === undefined ? 'any' : field.value ? 'yes' : 'no'
                }
                onChange={(e) => {
                  const v = e.target.value;
                  field.onChange(
                    v === 'any' ? undefined : v === 'yes' ? true : false
                  );
                }}
              >
                <Radio value="any">Any</Radio>
                <Radio value="yes">Only authenticated</Radio>
                <Radio value="no">Only anonymous</Radio>
              </Radio.Group>
            )}
          />
        </FormField>
        <FormField
          label="Named config flag"
          yamlPath="match.config_flag"
          helpText="Reserved for future dynamic-flag support. Today, unknown flags evaluate false. Known flags: maintenance_mode."
        >
          <Controller
            control={control}
            name="match.config_flag"
            render={({ field }) => (
              <Select
                allowClear
                placeholder="(none)"
                value={field.value}
                onChange={(v) => field.onChange(v)}
                options={[{ value: 'maintenance_mode', label: 'maintenance_mode' }]}
                style={{ width: '100%' }}
              />
            )}
          />
        </FormField>
      </div>

      {/* Action */}
      <div style={cardStyle}>
        <div style={cardLabel}>Action</div>
        <Controller
          control={control}
          name="action"
          render={({ field }) => (
            <Radio.Group
              value={kind}
              onChange={(e) => {
                const v = e.target.value;
                if (v === 'reject') {
                  field.onChange({ type: 'reject', status: 503, message: '' });
                } else {
                  field.onChange(v);
                }
              }}
            >
              <Radio value="allow-anonymous">Allow anonymous</Radio>
              <Radio value="deny">Deny (S3-style 403)</Radio>
              <Radio value="reject">Reject (custom status)</Radio>
              <Radio value="continue">Continue</Radio>
            </Radio.Group>
          )}
        />
        {(kind === 'deny' || kind === 'reject') && (
          <Alert
            type="warning"
            showIcon
            style={{ marginTop: 12 }}
            message={
              kind === 'deny'
                ? 'This will 403 all matching requests.'
                : 'This will return the configured status code for all matching requests.'
            }
          />
        )}
        {kind === 'reject' && typeof currentAction !== 'string' && (
          <div style={{ marginTop: 12 }}>
            <FormField
              label="Status code"
              yamlPath="action.status"
              helpText="HTTP status code. 4xx or 5xx only."
              examples={[503, 429, 401]}
              onExampleClick={(v) =>
                setValue('action', {
                  ...(currentAction as { type: 'reject'; status: number; message?: string }),
                  status: Number(v),
                })
              }
            >
              <InputNumber
                min={400}
                max={599}
                value={currentAction.status}
                onChange={(v) =>
                  setValue('action', {
                    ...(currentAction as { type: 'reject'; status: number; message?: string }),
                    status: Number(v ?? 503),
                  })
                }
                style={{ width: 120 }}
              />
            </FormField>
            <FormField
              label="Response message"
              yamlPath="action.message"
              helpText="Optional body for the rejection response. Max 4096 chars."
            >
              <Input
                value={currentAction.message}
                onChange={(e) =>
                  setValue('action', {
                    ...(currentAction as { type: 'reject'; status: number; message?: string }),
                    message: e.target.value,
                  })
                }
                placeholder="Maintenance in progress — try again later."
              />
            </FormField>
          </div>
        )}
      </div>
    </Modal>
  );
}
