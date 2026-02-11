import { useState, useRef, useCallback } from 'react';
import { uploadObject } from '../s3client';

interface Props {
  prefix: string;
  onPrefixChange: (prefix: string) => void;
  onUploaded: () => void;
}

export default function FileUpload({ prefix, onUploaded }: Props) {
  const [dragOver, setDragOver] = useState(false);
  const [uploading, setUploading] = useState(false);
  const inputRef = useRef<HTMLInputElement>(null);

  const handleFiles = useCallback(
    async (files: FileList) => {
      setUploading(true);
      try {
        for (const file of Array.from(files)) {
          const key = prefix ? `${prefix}${file.name}` : file.name;
          await uploadObject(key, file);
        }
        onUploaded();
      } catch (e) {
        console.error('Upload failed:', e);
      } finally {
        setUploading(false);
      }
    },
    [onUploaded, prefix]
  );

  const onDrop = (e: React.DragEvent) => {
    e.preventDefault();
    setDragOver(false);
    if (e.dataTransfer.files.length > 0) {
      handleFiles(e.dataTransfer.files);
    }
  };

  const displayPrefix = prefix || '/';

  return (
    <div className="upload-section">
      <div
        className={`upload-area ${dragOver ? 'drag-over' : ''}`}
        onDragOver={(e) => {
          e.preventDefault();
          setDragOver(true);
        }}
        onDragLeave={() => setDragOver(false)}
        onDrop={onDrop}
        onClick={() => inputRef.current?.click()}
      >
        <div className="upload-icon">&#8679;</div>
        <p>
          {uploading
            ? 'Uploading...'
            : `Drop files here or click to upload to ${displayPrefix}`}
        </p>
        <input
          ref={inputRef}
          type="file"
          multiple
          style={{ display: 'none' }}
          onChange={(e) => e.target.files && handleFiles(e.target.files)}
        />
      </div>
    </div>
  );
}
