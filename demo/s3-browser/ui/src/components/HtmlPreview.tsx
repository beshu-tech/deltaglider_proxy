import { useMemo, useState } from 'react';
import { Segmented, Switch, Typography } from 'antd';
import { useColors } from '../ThemeContext';
import { SPACE } from './shared-styles';
import { buildPreviewDocument, sandboxFlags, type HtmlPreviewOptions } from './htmlPreview';

const { Text } = Typography;

type View = 'rendered' | 'source';

interface HtmlPreviewProps {
  html: string;
  filename: string;
}

/**
 * An HTML object rendered in a sandboxed iframe (rules in `htmlPreview.ts`),
 * with its source one click away. Scripts and remote content are off until
 * the user turns them on for this preview.
 */
export default function HtmlPreview({ html, filename }: HtmlPreviewProps) {
  const colors = useColors();
  const [view, setView] = useState<View>('rendered');
  const [opts, setOpts] = useState<HtmlPreviewOptions>({ scripts: false, remote: false });
  const doc = useMemo(() => buildPreviewDocument(html, opts), [html, opts]);

  return (
    <div style={{ display: 'flex', flexDirection: 'column', gap: SPACE.sm }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: SPACE.md, flexWrap: 'wrap' }}>
        <Segmented<View>
          value={view}
          onChange={setView}
          options={[
            { label: 'Rendered', value: 'rendered' },
            { label: 'Source', value: 'source' },
          ]}
        />
        {view === 'rendered' && (
          <>
            <label style={{ display: 'flex', alignItems: 'center', gap: SPACE.xs }}>
              <Switch
                size="small"
                checked={opts.scripts}
                onChange={(scripts) => setOpts((o) => ({ ...o, scripts }))}
                aria-label="Run scripts"
              />
              <Text style={{ fontSize: 12 }}>Run scripts</Text>
            </label>
            <label style={{ display: 'flex', alignItems: 'center', gap: SPACE.xs }}>
              <Switch
                size="small"
                checked={opts.remote}
                onChange={(remote) => setOpts((o) => ({ ...o, remote }))}
                aria-label="Load remote content"
              />
              <Text style={{ fontSize: 12 }}>Load remote content</Text>
            </label>
            <Text
              type="secondary"
              style={{ fontSize: 12, marginLeft: 'auto' }}
              title="The page runs isolated from this console: it cannot read your session or credentials, and it cannot send requests."
            >
              Isolated preview
            </Text>
          </>
        )}
      </div>

      {view === 'rendered' ? (
        <iframe
          // A new document when an option changes: scripts that already ran
          // and loads that were already refused start over.
          key={`${opts.scripts}-${opts.remote}`}
          title={`Preview of ${filename}`}
          sandbox={sandboxFlags(opts)}
          srcDoc={doc}
          referrerPolicy="no-referrer"
          style={{
            width: '100%',
            height: '70vh',
            border: `1px solid ${colors.BORDER}`,
            borderRadius: 8,
            // Pages assume a white canvas; the dark theme must not show through.
            background: '#fff',
          }}
        />
      ) : (
        <pre
          aria-label={`Source of ${filename}`}
          style={{
            background: colors.BG_BASE,
            border: `1px solid ${colors.BORDER}`,
            borderRadius: 8,
            padding: SPACE.md,
            margin: 0,
            height: '70vh',
            overflow: 'auto',
            fontFamily: 'var(--font-mono)',
            fontSize: 12,
            lineHeight: 1.6,
            whiteSpace: 'pre-wrap',
            wordBreak: 'break-word',
            color: colors.TEXT_PRIMARY,
          }}
        >
          {html}
        </pre>
      )}
    </div>
  );
}
