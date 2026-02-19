import { useState } from 'react';
import { ExperimentOutlined, LoadingOutlined } from '@ant-design/icons';
import { uploadObject } from '../s3client';
import { useColors } from '../ThemeContext';

interface Props {
  onDone: () => void;
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

export default function DemoDataGenerator({ onDone }: Props) {
  const [generating, setGenerating] = useState(false);
  const [progress, setProgress] = useState('');

  const generate = async () => {
    setGenerating(true);
    try {
      const base = generateBaseData(50_000);
      for (let v = 1; v <= 5; v++) {
        setProgress(`Uploading version ${v}/5...`);
        const data = mutateData(base, v);
        await uploadObject(
          `demo-releases/app-v${v}.zip`,
          data.buffer as ArrayBuffer
        );
      }
      setProgress('Done!');
      onDone();
      setTimeout(() => setProgress(''), 2000);
    } catch (e) {
      setProgress('Error generating demo data');
      console.error(e);
    } finally {
      setGenerating(false);
    }
  };

  const { TEXT_PRIMARY, TEXT_SECONDARY } = useColors();

  return (
    <div>
      <button
        className="btn-reset"
        onClick={generate}
        disabled={generating}
        style={{
          gap: 10,
          padding: '8px 6px',
          color: TEXT_SECONDARY,
          fontSize: 13,
          width: '100%',
          transition: 'color 0.15s',
          fontFamily: "var(--font-ui)",
          opacity: generating ? 0.6 : 1,
        }}
        onMouseEnter={(e) => { if (!generating) e.currentTarget.style.color = TEXT_PRIMARY; }}
        onMouseLeave={(e) => { e.currentTarget.style.color = TEXT_SECONDARY; }}
      >
        {generating
          ? <LoadingOutlined aria-hidden="true" style={{ fontSize: 14, width: 22, textAlign: 'center', display: 'inline-flex', justifyContent: 'center' }} />
          : <ExperimentOutlined aria-hidden="true" style={{ fontSize: 14, width: 22, textAlign: 'center', display: 'inline-flex', justifyContent: 'center' }} />
        }
        <span>{progress || 'Demo Data'}</span>
      </button>
    </div>
  );
}
