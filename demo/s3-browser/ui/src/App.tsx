import { useState, useEffect, useCallback } from 'react';
import { getEndpoint, setEndpoint } from './s3client';
import StorageStats from './components/StorageStats';
import FileBrowser from './components/FileBrowser';

export default function App() {
  const [endpoint, setEp] = useState(getEndpoint());
  const [refreshTrigger, setRefreshTrigger] = useState(0);

  const handleEndpointChange = (val: string) => {
    setEp(val);
    setEndpoint(val);
  };

  const refresh = useCallback(() => {
    setRefreshTrigger((k) => k + 1);
  }, []);

  // Auto-refresh every 5 seconds
  useEffect(() => {
    const id = setInterval(refresh, 5000);
    return () => clearInterval(id);
  }, [refresh]);

  return (
    <div className="app">
      <header className="app-header">
        <h1>
          <span>DeltaGlider</span> S3 Browser
        </h1>
        <div className="endpoint-cfg">
          <label>Proxy:</label>
          <input
            value={endpoint}
            onChange={(e) => handleEndpointChange(e.target.value)}
            placeholder="http://localhost:9002"
          />
        </div>
      </header>

      <StorageStats refreshTrigger={refreshTrigger} />
      <FileBrowser refreshTrigger={refreshTrigger} onMutate={refresh} />
    </div>
  );
}
