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
 *   * All match conditions in ONE card (methods, source IPs, bucket, key
 *     pattern, signed/anonymous). Source IPs is ONE text field (one IP or
 *     CIDR per line) that maps onto `source_ip` / `source_ip_list` via
 *     `sourceIpMatch` — the two YAML keys used to be two mutually exclusive
 *     inputs. `config_flag` shows only when a rule already has one (no flag
 *     can be switched on yet, so such a rule never matches).
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
import { useEffect, useMemo, useState } from 'react';
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
import { sourceIpMatch, sourceIpText } from '../sourceIpField';

/** Tighter field rhythm than FormField's page default: keeps the modal short. */
const FIELD_GAP: React.CSSProperties = { marginBottom: 16 };
const TWO_COLUMNS: React.CSSProperties = {
  display: 'grid',
  gridTemplateColumns: 'repeat(auto-fit, minmax(220px, 1fr))',
  columnGap: 16,
};

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

  // The Source IPs text field. Seeded from the rule on open; its text is the
  // truth for the textarea (so a half-typed line is not reformatted), and
  // every change writes the derived source_ip / source_ip_list keys.
  const originalIps = useMemo(
    () => ({ source_ip: defaults.match.source_ip, source_ip_list: defaults.match.source_ip_list }),
    [defaults]
  );
  const [ipText, setIpText] = useState(() => sourceIpText(originalIps));
  useEffect(() => {
    if (open) setIpText(sourceIpText(originalIps));
  }, [open, originalIps]);
  const configFlag = watch('match.config_flag');

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
        message: `A rule named "${data.name}" already exists.`,
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
      title={initial ? `Edit request rule: ${initial.name}` : 'Add request rule'}
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
            {initial ? 'Save' : 'Add rule'}
          </Button>
        </Space>
      }
    >
      {/* Name */}
      <FormField
        label="Name"
        yamlPath="admission.blocks[].name"
        helpText="A unique name. Letters, digits, and the characters _ : . - only."
        style={FIELD_GAP}
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

      {/* Match: every condition in one card. Empty = matches everything. */}
      <div style={cardStyle}>
        <div style={cardLabel}>When a request matches — leave a condition empty to match any value</div>
        <FormField label="HTTP methods" yamlPath="match.method" style={FIELD_GAP}>
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

        {/* ONE field for source addresses: a single IP or many IPs/networks,
            one per line. sourceIpMatch() picks source_ip or source_ip_list. */}
        <FormField
          label="Source IPs"
          yamlPath="match.source_ip_list"
          helpText="One IP address or network (CIDR) per line. Up to 4096 entries."
          style={FIELD_GAP}
        >
          <Input.TextArea
            rows={2}
            autoSize={{ minRows: 2, maxRows: 8 }}
            placeholder={'203.0.113.5\n198.51.100.0/24'}
            value={ipText}
            onChange={(e) => {
              setIpText(e.target.value);
              const m = sourceIpMatch(e.target.value, originalIps);
              setValue('match.source_ip', m.source_ip, { shouldValidate: true });
              setValue('match.source_ip_list', m.source_ip_list, { shouldValidate: true });
            }}
            style={{ fontFamily: 'var(--font-mono)', fontSize: 13 }}
          />
          {(errors.match?.source_ip_list || errors.match?.source_ip) && (
            <Text type="danger" style={{ fontSize: 12 }}>
              {errors.match?.source_ip_list?.message ?? errors.match?.source_ip?.message}
            </Text>
          )}
        </FormField>

        <div style={TWO_COLUMNS}>
          <FormField label="Bucket" yamlPath="match.bucket" style={FIELD_GAP}>
            <Controller
              control={control}
              name="match.bucket"
              render={({ field }) => (
                <Input
                  placeholder="any bucket"
                  value={field.value ?? ''}
                  onChange={(e) => field.onChange(e.target.value || undefined)}
                  onBlur={field.onBlur}
                  ref={field.ref}
                />
              )}
            />
          </FormField>
          <FormField
            label="Object key pattern"
            yamlPath="match.path_glob"
            examples={['*.zip', 'builds/**']}
            onExampleClick={(v) => setValue('match.path_glob', String(v))}
            style={FIELD_GAP}
          >
            <Controller
              control={control}
              name="match.path_glob"
              render={({ field }) => (
                <Input
                  placeholder="any key"
                  value={field.value ?? ''}
                  onChange={(e) => field.onChange(e.target.value || undefined)}
                  onBlur={field.onBlur}
                  ref={field.ref}
                />
              )}
            />
          </FormField>
        </div>

        <FormField label="Signed request" yamlPath="match.authenticated" style={{ marginBottom: 0 }}>
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
                <Radio value="any">Either</Radio>
                <Radio value="yes">Signed only</Radio>
                <Radio value="no">Anonymous only</Radio>
              </Radio.Group>
            )}
          />
        </FormField>

        {/* No flag can be switched on yet (the evaluator treats every flag as
            off), so the field is shown only to clear one a rule already has. */}
        {configFlag && (
          <FormField
            label="Config flag"
            yamlPath="match.config_flag"
            helpText="No flag can be switched on yet, so a rule with a flag never matches. Clear it to make the rule work."
            style={{ marginTop: 16, marginBottom: 0 }}
          >
            <Controller
              control={control}
              name="match.config_flag"
              render={({ field }) => (
                <Select
                  allowClear
                  value={field.value}
                  onChange={(v) => field.onChange(v)}
                  options={[{ value: field.value ?? '', label: field.value ?? '' }]}
                  style={{ width: '100%' }}
                />
              )}
            />
          </FormField>
        )}
      </div>

      {/* Action */}
      <div style={cardStyle}>
        <div style={cardLabel}>Then</div>
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
              <Radio value="allow-anonymous">Allow without credentials</Radio>
              <Radio value="deny">Deny (403)</Radio>
              <Radio value="reject">Reject with a custom status</Radio>
              <Radio value="continue">Continue to authentication</Radio>
            </Radio.Group>
          )}
        />
        {(kind === 'deny' || kind === 'reject') && (
          <Alert
            type="warning"
            showIcon
            style={{ marginTop: 12 }}
            title={
              kind === 'deny'
                ? 'Every matching request gets a 403 Access Denied response.'
                : 'Every matching request gets the status code below.'
            }
          />
        )}
        {kind === 'reject' && typeof currentAction !== 'string' && (
          <div style={{ marginTop: 12 }}>
            <FormField
              label="Status code"
              yamlPath="action.status"
              helpText="A 4xx or 5xx status code."
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
              helpText="Optional. The message in the response body, up to 4096 characters."
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
