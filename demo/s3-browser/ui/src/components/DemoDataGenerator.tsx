import { useEffect, useRef, useState } from 'react';
import { ExperimentOutlined, LoadingOutlined } from '@ant-design/icons';
import { uploadObject } from '../s3client';
import { useColors } from '../ThemeContext';

const MENU_ICON_STYLE: React.CSSProperties = { fontSize: 14, width: 22, textAlign: 'center', display: 'inline-flex', justifyContent: 'center' };

interface Props {
  onDone: () => void;
  variant?: 'inline' | 'empty-state';
  label?: string;
}

function generateBaseData(size: number): Uint8Array {
  const data = new Uint8Array(size);
  for (let i = 0; i < size; i++) {
    data[i] = (i * 7 + 13) & 0xff;
  }
  return data;
}

function mutateData(base: Uint8Array, version: number): Uint8Array {
  const copy = new Uint8Array(base);
  const mutations = 50 + version * 30;
  for (let i = 0; i < mutations; i++) {
    const idx = (version * 997 + i * 131) % copy.length;
    copy[idx] = (copy[idx] + version + i) & 0xff;
  }
  return copy;
}

export default function DemoDataGenerator({ onDone, variant = 'inline', label = 'Demo Data' }: Props) {
  const [generating, setGenerating] = useState(false);
  const [progress, setProgress] = useState('');
  const mountedRef = useRef(true);

  useEffect(() => {
    return () => {
      mountedRef.current = false;
    };
  }, []);

  const generate = async () => {
    setGenerating(true);
    try {
      const base = generateBaseData(50_000);
      for (let v = 1; v <= 5; v++) {
        if (!mountedRef.current) return;
        setProgress(`Uploading version ${v}/5...`);
        const data = mutateData(base, v);
        await uploadObject(
          `demo-releases/app-v${v}.zip`,
          data.buffer as ArrayBuffer
        );
      }
      setProgress('Done!');
      onDone();
      window.setTimeout(() => {
        if (mountedRef.current) setProgress('');
      }, 2000);
    } catch (e) {
      if (mountedRef.current) setProgress('Error generating demo data');
      console.error(e);
    } finally {
      if (mountedRef.current) setGenerating(false);
    }
  };

  const { TEXT_PRIMARY, TEXT_SECONDARY, ACCENT_BLUE } = useColors();
  const isEmptyState = variant === 'empty-state';

  return (
    <div>
      <button
        className="btn-reset"
        onClick={generate}
        disabled={generating}
        style={{
          gap: isEmptyState ? 10 : 8,
          padding: isEmptyState ? '4px 8px' : '6px 6px',
          color: isEmptyState ? ACCENT_BLUE : TEXT_SECONDARY,
          fontSize: isEmptyState ? 13 : 11,
          fontWeight: isEmptyState ? 600 : 400,
          width: isEmptyState ? 'auto' : '100%',
          border: 'none',
          borderRadius: isEmptyState ? 8 : undefined,
          background: 'transparent',
          transition: 'color 0.15s',
          fontFamily: "var(--font-ui)",
          opacity: generating ? 0.6 : (isEmptyState ? 1 : 0.7),
        }}
        onMouseEnter={(e) => { if (!generating) e.currentTarget.style.color = TEXT_PRIMARY; }}
        onMouseLeave={(e) => { e.currentTarget.style.color = isEmptyState ? ACCENT_BLUE : TEXT_SECONDARY; }}
      >
        {generating
          ? <LoadingOutlined aria-hidden="true" style={MENU_ICON_STYLE} />
          : <ExperimentOutlined aria-hidden="true" style={MENU_ICON_STYLE} />
        }
        <span>{progress || label}</span>
      </button>
    </div>
  );
}
