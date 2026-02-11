import { useState } from 'react';
import { uploadObject } from '../s3client';

interface Props {
  onDone: () => void;
}

function generateBaseData(size: number): Uint8Array {
  const data = new Uint8Array(size);
  // Create structured data that compresses well as delta
  for (let i = 0; i < size; i++) {
    data[i] = (i * 7 + 13) & 0xff;
  }
  return data;
}

function mutateData(base: Uint8Array, version: number): Uint8Array {
  const copy = new Uint8Array(base);
  // Apply small mutations proportional to version number
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
      const base = generateBaseData(50_000); // 50KB base file
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

  return (
    <div className="demo-gen">
      <button className="btn" onClick={generate} disabled={generating}>
        Generate Demo Data
      </button>
      {progress && <span className="progress-text">{progress}</span>}
    </div>
  );
}
