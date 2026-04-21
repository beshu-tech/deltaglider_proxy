/**
 * MonacoYamlEditor — lazy-loaded Monaco + monaco-yaml wrapper.
 *
 * Wave 2 foundation for every YAML surface in the new admin UI. The
 * editor binds one JSON Schema per instance (fetched via
 * `getSectionSchema(section)`) so monaco-yaml's lint, hover-docs, and
 * autocomplete are scoped to what the editor covers.
 *
 * ## Performance note
 *
 * Monaco's bundle is ~2 MB minified. We lazy-import it inside the
 * component so the admin-UI's initial bundle stays small for operators
 * who never open a YAML view. The editor itself is a React import, so
 * the `loader.init` pattern can't be used — we dynamic-import
 * `monaco-editor` and `monaco-yaml` together the first time a
 * `MonacoYamlEditor` mounts.
 *
 * ## Read-only mobile fallback
 *
 * Editing YAML on a phone is a bad UX and Monaco's mobile support is
 * weak. When the viewport is < 600px the editor renders in `readOnly`
 * mode with a banner explaining the limitation (§10.4 of the plan).
 */
import { useEffect, useRef, useState } from 'react';
import type { CSSProperties } from 'react';
import { Alert, Spin } from 'antd';

interface Props {
  /** Current YAML text. */
  value: string;
  /** Fires on every edit. Parent persists into form state. */
  onChange?: (value: string) => void;
  /**
   * JSON Schema (from `getSectionSchema(section)` or similar) used by
   * monaco-yaml for inline lint, hover, and autocomplete.
   * Optional — editor works as plain YAML without it.
   */
  schema?: unknown;
  /** Logical identifier for this editor, used as the YAML schema URI. */
  schemaId?: string;
  /** CSS height; editor fills its container. Default: 400px. */
  height?: CSSProperties['height'];
  /** Read-only mode — disables editing but keeps syntax highlighting. */
  readOnly?: boolean;
  /** Theme override; omitted = follow the global `prefers-color-scheme`. */
  theme?: 'vs' | 'vs-dark';
}

// Module-scoped singleton so repeated mounts don't re-download Monaco.
let monacoBundle: {
  monaco: typeof import('monaco-editor');
  monacoYaml: typeof import('monaco-yaml');
} | null = null;

async function loadMonaco() {
  if (monacoBundle) return monacoBundle;
  const [monaco, monacoYaml] = await Promise.all([
    import('monaco-editor'),
    import('monaco-yaml'),
  ]);
  monacoBundle = { monaco, monacoYaml };
  return monacoBundle;
}

export default function MonacoYamlEditor({
  value,
  onChange,
  schema,
  schemaId = 'admin-yaml',
  height = 400,
  readOnly = false,
  theme,
}: Props) {
  const containerRef = useRef<HTMLDivElement>(null);
  const editorRef = useRef<import('monaco-editor').editor.IStandaloneCodeEditor | null>(null);
  const modelRef = useRef<import('monaco-editor').editor.ITextModel | null>(null);
  const [ready, setReady] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [mobile, setMobile] = useState(false);

  // Detect mobile viewport once — editor switches to read-only below
  // 600px per §10.4.
  useEffect(() => {
    const check = () => setMobile(window.innerWidth < 600);
    check();
    window.addEventListener('resize', check);
    return () => window.removeEventListener('resize', check);
  }, []);

  // Mount + unmount lifecycle. Monaco's editor instance is attached
  // directly to the container `<div>` (not a React child); we tear it
  // down on unmount so the DOM stays clean.
  useEffect(() => {
    let cancelled = false;
    async function init() {
      try {
        const { monaco, monacoYaml } = await loadMonaco();
        if (cancelled || !containerRef.current) return;

        // Configure monaco-yaml with the scoped schema once per mount.
        // Schema changes between mounts get picked up because
        // `configureMonacoYaml` is effectively "replace all registered
        // schemas".
        if (schema) {
          const uri = `file:///${schemaId}.yaml`;
          monacoYaml.configureMonacoYaml(monaco, {
            validate: true,
            enableSchemaRequest: false,
            hover: true,
            completion: true,
            format: true,
            schemas: [
              {
                uri: `inmemory://schema/${schemaId}`,
                fileMatch: [uri],
                schema: schema as object,
              },
            ],
          });
          modelRef.current = monaco.editor.createModel(value, 'yaml', monaco.Uri.parse(uri));
        } else {
          modelRef.current = monaco.editor.createModel(value, 'yaml');
        }

        const resolvedTheme =
          theme ||
          (window.matchMedia('(prefers-color-scheme: dark)').matches ? 'vs-dark' : 'vs');

        editorRef.current = monaco.editor.create(containerRef.current, {
          model: modelRef.current,
          theme: resolvedTheme,
          readOnly: readOnly || mobile,
          minimap: { enabled: false },
          automaticLayout: true,
          fontSize: 13,
          fontFamily: 'var(--font-mono)',
          scrollBeyondLastLine: false,
          wordWrap: 'on',
          tabSize: 2,
        });

        editorRef.current.onDidChangeModelContent(() => {
          if (onChange && modelRef.current) {
            onChange(modelRef.current.getValue());
          }
        });

        setReady(true);
      } catch (e) {
        setError(e instanceof Error ? e.message : 'Failed to load YAML editor');
      }
    }
    init();
    return () => {
      cancelled = true;
      editorRef.current?.dispose();
      modelRef.current?.dispose();
      editorRef.current = null;
      modelRef.current = null;
    };
    // `schema`, `schemaId`, `readOnly`, `theme`, `mobile` only matter on
    // mount — changing them requires remounting the component. Parent
    // passes a fresh `key` prop when the schema URI changes.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // Keep Monaco's model in sync when the `value` prop drifts (e.g.
  // parent reverted after Apply). We skip the echo-update that would
  // otherwise happen right after the operator's edit.
  useEffect(() => {
    if (!modelRef.current) return;
    if (modelRef.current.getValue() !== value) {
      modelRef.current.setValue(value);
    }
  }, [value]);

  return (
    <div style={{ position: 'relative', height }}>
      {error && (
        <Alert
          type="error"
          message="YAML editor failed to load"
          description={error}
          showIcon
          style={{ marginBottom: 8 }}
        />
      )}
      {mobile && !readOnly && (
        <Alert
          type="info"
          message="YAML editor is read-only on small screens. Use a desktop browser to edit."
          showIcon
          banner
          style={{ marginBottom: 8 }}
        />
      )}
      <div
        ref={containerRef}
        style={{
          width: '100%',
          height: '100%',
          border: '1px solid var(--color-border)',
          borderRadius: 8,
          overflow: 'hidden',
        }}
      />
      {!ready && !error && (
        <div
          style={{
            position: 'absolute',
            inset: 0,
            display: 'flex',
            alignItems: 'center',
            justifyContent: 'center',
            background: 'rgba(0,0,0,0.04)',
            pointerEvents: 'none',
          }}
        >
          <Spin tip="Loading YAML editor..." />
        </div>
      )}
    </div>
  );
}
