import { useState, useEffect } from 'react';
import { getStats } from '../s3client';
import { formatBytes } from '../utils';
import type { StorageStats as Stats } from '../types';

interface Props {
  refreshTrigger: number;
}

export default function StorageStats({ refreshTrigger }: Props) {
  const [stats, setStats] = useState<Stats | null>(null);

  useEffect(() => {
    getStats()
      .then(setStats)
      .catch(() => setStats(null));
  }, [refreshTrigger]);

  if (!stats) {
    return (
      <div className="stats-card">
        <div className="stat">
          <div className="stat-value">--</div>
          <div className="stat-label">Objects</div>
        </div>
        <div className="stat">
          <div className="stat-value">--</div>
          <div className="stat-label">Original Size</div>
        </div>
        <div className="stat">
          <div className="stat-value">--</div>
          <div className="stat-label">Stored Size</div>
        </div>
        <div className="stat">
          <div className="stat-value savings">--</div>
          <div className="stat-label">Savings</div>
        </div>
      </div>
    );
  }

  return (
    <div className="stats-card">
      <div className="stat">
        <div className="stat-value">{stats.total_objects}</div>
        <div className="stat-label">Objects</div>
      </div>
      <div className="stat">
        <div className="stat-value">{formatBytes(stats.total_original_size)}</div>
        <div className="stat-label">Original Size</div>
      </div>
      <div className="stat">
        <div className="stat-value">{formatBytes(stats.total_stored_size)}</div>
        <div className="stat-label">Stored Size</div>
      </div>
      <div className="stat">
        <div className="stat-value savings">
          {stats.savings_percentage.toFixed(1)}%
        </div>
        <div className="stat-label">Savings</div>
      </div>
    </div>
  );
}
