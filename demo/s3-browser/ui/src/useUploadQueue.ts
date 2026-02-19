import { useState, useEffect, useCallback, useRef } from 'react';
import { uploadObject } from './s3client';

export interface UploadItem {
  id: string;
  file: File;
  status: 'pending' | 'uploading' | 'done' | 'error';
  originalSize: number;
  error?: string;
}

export interface UploadStats {
  uploaded: number;
  originalSize: number;
  storedSize: number;
}

export default function useUploadQueue(destination: string) {
  const [queue, setQueue] = useState<UploadItem[]>([]);
  const [stats, setStats] = useState<UploadStats>({ uploaded: 0, originalSize: 0, storedSize: 0 });
  const processingRef = useRef(false);

  const addFiles = useCallback((files: FileList | File[]) => {
    const items: UploadItem[] = Array.from(files).map((file) => ({
      id: `${Date.now()}-${Math.random().toString(36).slice(2, 8)}`,
      file,
      status: 'pending' as const,
      originalSize: file.size,
    }));
    setQueue((prev) => [...prev, ...items]);
  }, []);

  // Process one pending item at a time
  useEffect(() => {
    if (processingRef.current) return;
    const pending = queue.find((item) => item.status === 'pending');
    if (!pending) return;

    processingRef.current = true;

    setQueue((prev) =>
      prev.map((item) => (item.id === pending.id ? { ...item, status: 'uploading' as const } : item))
    );

    const key = destination ? `${destination}${pending.file.name}` : pending.file.name;

    uploadObject(key, pending.file)
      .then(() => {
        setQueue((prev) =>
          prev.map((item) =>
            item.id === pending.id ? { ...item, status: 'done' as const } : item
          )
        );
        setStats((prev) => ({
          uploaded: prev.uploaded + 1,
          originalSize: prev.originalSize + pending.originalSize,
          storedSize: prev.storedSize + pending.originalSize,
        }));
      })
      .catch((err) => {
        setQueue((prev) =>
          prev.map((item) =>
            item.id === pending.id
              ? { ...item, status: 'error' as const, error: err instanceof Error ? err.message : 'Upload failed' }
              : item
          )
        );
      })
      .finally(() => {
        processingRef.current = false;
      });
  }, [queue, destination]);

  const clearCompleted = useCallback(() => {
    setQueue((prev) => prev.filter((item) => item.status !== 'done' && item.status !== 'error'));
  }, []);

  const pendingCount = queue.filter((i) => i.status === 'pending' || i.status === 'uploading').length;

  const savings = stats.originalSize > 0
    ? Math.max(0, ((stats.originalSize - stats.storedSize) / stats.originalSize) * 100)
    : 0;

  return { queue, stats, savings, pendingCount, addFiles, clearCompleted };
}
