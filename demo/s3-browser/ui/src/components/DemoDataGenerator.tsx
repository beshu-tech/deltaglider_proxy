import { useState } from 'react';
import { Button, Typography } from 'antd';
import { ExperimentOutlined } from '@ant-design/icons';
import { uploadObject } from '../s3client';

const { Text } = Typography;

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

  return (
    <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
      <Button
        icon={<ExperimentOutlined />}
        onClick={generate}
        loading={generating}
        block
      >
        Demo Data
      </Button>
      {progress && <Text type="secondary" style={{ fontSize: 12 }}>{progress}</Text>}
    </div>
  );
}
