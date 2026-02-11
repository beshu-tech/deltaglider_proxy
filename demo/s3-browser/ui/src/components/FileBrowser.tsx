import { useState, useEffect, useCallback } from 'react';
import { listObjects, deleteObjects } from '../s3client';
import type { S3Object } from '../types';
import Breadcrumb from './Breadcrumb';
import FileList from './FileList';
import FileUpload from './FileUpload';
import ObjectDetail from './ObjectDetail';
import DemoDataGenerator from './DemoDataGenerator';

interface Props {
  refreshTrigger: number;
  onMutate: () => void;
}

export default function FileBrowser({ refreshTrigger, onMutate }: Props) {
  const [objects, setObjects] = useState<S3Object[]>([]);
  const [folders, setFolders] = useState<string[]>([]);
  const [prefix, setPrefix] = useState('');
  const [loading, setLoading] = useState(true);
  const [selected, setSelected] = useState<S3Object | null>(null);
  const [selectedKeys, setSelectedKeys] = useState<Set<string>>(new Set());
  const [deleting, setDeleting] = useState(false);

  const load = useCallback(() => {
    setLoading(true);
    listObjects(prefix)
      .then(({ objects: objs, folders: dirs }) => {
        setObjects(objs);
        setFolders(dirs);
        setSelected((prev) => {
          if (!prev) return null;
          return objs.find((o) => o.key === prev.key) || null;
        });
        setSelectedKeys((prev) => {
          const existing = new Set(objs.map((o) => o.key));
          const next = new Set<string>();
          for (const k of prev) {
            if (existing.has(k)) next.add(k);
          }
          return next.size === prev.size ? prev : next;
        });
      })
      .catch(() => {
        setObjects([]);
        setFolders([]);
      })
      .finally(() => setLoading(false));
  }, [prefix]);

  useEffect(load, [load, refreshTrigger]);

  const handleMutate = () => {
    load();
    onMutate();
  };

  const navigate = (newPrefix: string) => {
    setPrefix(newPrefix);
    setSelected(null);
    setSelectedKeys(new Set());
  };

  const toggleKey = (key: string) => {
    setSelectedKeys((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  };

  const toggleAll = () => {
    if (selectedKeys.size === objects.length) {
      setSelectedKeys(new Set());
    } else {
      setSelectedKeys(new Set(objects.map((o) => o.key)));
    }
  };

  const handleBulkDelete = async () => {
    if (selectedKeys.size === 0) return;
    setDeleting(true);
    try {
      await deleteObjects(Array.from(selectedKeys));
      setSelectedKeys(new Set());
      setSelected(null);
      handleMutate();
    } catch (e) {
      console.error('Bulk delete failed:', e);
    } finally {
      setDeleting(false);
    }
  };

  const isEmpty = objects.length === 0 && folders.length === 0;

  return (
    <div>
      <FileUpload
        prefix={prefix}
        onPrefixChange={navigate}
        onUploaded={handleMutate}
      />

      <Breadcrumb prefix={prefix} onNavigate={navigate} />

      <div className="actions-bar">
        <button className="btn" onClick={handleMutate}>
          Refresh
        </button>
        <DemoDataGenerator onDone={handleMutate} />
        {selectedKeys.size > 0 && (
          <button
            className="btn btn-danger"
            onClick={handleBulkDelete}
            disabled={deleting}
          >
            {deleting
              ? 'Deleting...'
              : `Delete ${selectedKeys.size} selected`}
          </button>
        )}
      </div>

      {loading && isEmpty ? (
        <div className="loading">Loading objects...</div>
      ) : isEmpty ? (
        <div className="empty-state">
          <div className="icon">&#128451;</div>
          <p>
            {prefix
              ? 'This folder is empty.'
              : 'No objects yet. Upload files or generate demo data.'}
          </p>
        </div>
      ) : (
        <FileList
          objects={objects}
          folders={folders}
          prefix={prefix}
          selected={selected}
          onSelect={setSelected}
          onNavigate={navigate}
          selectedKeys={selectedKeys}
          onToggleKey={toggleKey}
          onToggleAll={toggleAll}
        />
      )}

      {selected && (
        <ObjectDetail
          object={selected}
          onClose={() => setSelected(null)}
          onDeleted={handleMutate}
        />
      )}
    </div>
  );
}
