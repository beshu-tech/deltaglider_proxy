import { useState, useEffect, useCallback, useRef } from 'react';
import { Layout, Spin, Empty, Grid } from 'antd';
import { listObjects, deleteObjects, uploadObject, getBucket } from './s3client';
import type { S3Object } from './types';
import TopBar from './components/TopBar';
import Sidebar from './components/Sidebar';
import Toolbar from './components/Toolbar';
import ObjectTable from './components/ObjectTable';
import InspectorPanel from './components/InspectorPanel';
import Footer from './components/Footer';
import DropZone from './components/DropZone';

const { Content } = Layout;
const { useBreakpoint } = Grid;

interface Props {
  isDark: boolean;
  onToggleTheme: () => void;
}

export default function App({ isDark, onToggleTheme }: Props) {
  const [refreshTrigger, setRefreshTrigger] = useState(0);
  const [objects, setObjects] = useState<S3Object[]>([]);
  const [folders, setFolders] = useState<string[]>([]);
  const [prefix, setPrefix] = useState('');
  const [loading, setLoading] = useState(true);
  const [selected, setSelected] = useState<S3Object | null>(null);
  const [selectedKeys, setSelectedKeys] = useState<Set<string>>(new Set());
  const [deleting, setDeleting] = useState(false);
  const [uploading, setUploading] = useState(false);
  const [connected, setConnected] = useState(true);
  const [bucket, setBucketState] = useState(getBucket());
  const [siderOpen, setSiderOpen] = useState(false);
  const prefixRef = useRef(prefix);
  prefixRef.current = prefix;
  const screens = useBreakpoint();

  const refresh = useCallback(() => {
    setRefreshTrigger((k) => k + 1);
  }, []);

  const load = useCallback(() => {
    setLoading(true);
    listObjects(prefix)
      .then(({ objects: objs, folders: dirs }) => {
        setObjects(objs);
        setFolders(dirs);
        setConnected(true);
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
        setConnected(false);
      })
      .finally(() => setLoading(false));
  }, [prefix, bucket]);

  useEffect(load, [load, refreshTrigger]);

  useEffect(() => {
    const id = setInterval(refresh, 5000);
    return () => clearInterval(id);
  }, [refresh]);

  const handleMutate = () => {
    load();
    refresh();
  };

  const navigate = (newPrefix: string) => {
    setPrefix(newPrefix);
    setSelected(null);
    setSelectedKeys(new Set());
  };

  const handleBucketChange = (newBucket: string) => {
    setBucketState(newBucket);
    setPrefix('');
    setSelected(null);
    setSelectedKeys(new Set());
    setRefreshTrigger((k) => k + 1);
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

  const handleUploadFiles = useCallback(async (files: FileList) => {
    const currentPrefix = prefixRef.current;
    setUploading(true);
    try {
      for (const file of Array.from(files)) {
        const key = currentPrefix ? `${currentPrefix}${file.name}` : file.name;
        await uploadObject(key, file);
      }
      setRefreshTrigger((k) => k + 1);
    } catch (e) {
      console.error('Upload failed:', e);
    } finally {
      setUploading(false);
    }
  }, []);

  const isMobile = !screens.md;
  const isEmpty = objects.length === 0 && folders.length === 0;

  return (
    <Layout style={{ minHeight: '100vh' }}>
      <TopBar
        prefix={prefix}
        onNavigate={navigate}
        connected={connected}
        isDark={isDark}
        onToggleTheme={onToggleTheme}
        isMobile={isMobile}
        onMenuClick={() => setSiderOpen(true)}
      />

      <Layout style={{ flex: 1 }}>
        <Sidebar
          folders={folders}
          prefix={prefix}
          onNavigate={(p) => { navigate(p); setSiderOpen(false); }}
          onUploadFiles={handleUploadFiles}
          uploading={uploading}
          onMutate={handleMutate}
          refreshTrigger={refreshTrigger}
          onBucketChange={handleBucketChange}
          open={siderOpen}
          onClose={() => setSiderOpen(false)}
        />

        <Content style={{ display: 'flex', flexDirection: 'column', minHeight: 0 }}>
          <Toolbar
            onRefresh={handleMutate}
            onUploadFiles={handleUploadFiles}
            uploading={uploading}
            selectedCount={selectedKeys.size}
            deleting={deleting}
            onBulkDelete={handleBulkDelete}
            isMobile={isMobile}
          />

          <div style={{ flex: 1, overflow: 'auto' }}>
            {loading && isEmpty ? (
              <div style={{ display: 'flex', justifyContent: 'center', alignItems: 'center', padding: '64px 0' }}>
                <Spin tip="Loading objects..." />
              </div>
            ) : isEmpty ? (
              <Empty
                description={
                  prefix
                    ? 'This folder is empty.'
                    : 'No objects yet. Upload files or generate demo data.'
                }
                style={{ padding: '64px 0' }}
              />
            ) : (
              <ObjectTable
                objects={objects}
                folders={folders}
                prefix={prefix}
                selected={selected}
                onSelect={setSelected}
                onNavigate={navigate}
                selectedKeys={selectedKeys}
                onToggleKey={toggleKey}
                onToggleAll={toggleAll}
                isMobile={isMobile}
              />
            )}
          </div>
        </Content>
      </Layout>

      <Footer connected={connected} objectCount={objects.length} isMobile={isMobile} />

      <InspectorPanel
        object={selected}
        onClose={() => setSelected(null)}
        onDeleted={handleMutate}
        isMobile={isMobile}
      />

      <DropZone onDrop={handleUploadFiles} prefix={prefix} />
    </Layout>
  );
}
