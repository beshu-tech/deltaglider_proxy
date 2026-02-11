import type { S3Object } from '../types';
import { formatBytes, badgeClass, savingsPercent, displayName } from '../utils';

interface Props {
  objects: S3Object[];
  folders: string[];
  prefix: string;
  selected: S3Object | null;
  onSelect: (obj: S3Object) => void;
  onNavigate: (prefix: string) => void;
  selectedKeys: Set<string>;
  onToggleKey: (key: string) => void;
  onToggleAll: () => void;
}

export default function FileList({
  objects,
  folders,
  prefix,
  selected,
  onSelect,
  onNavigate,
  selectedKeys,
  onToggleKey,
  onToggleAll,
}: Props) {
  const allChecked = objects.length > 0 && selectedKeys.size === objects.length;
  const someChecked = selectedKeys.size > 0 && selectedKeys.size < objects.length;

  return (
    <table className="file-table">
      <thead>
        <tr>
          <th style={{ width: 36 }}>
            <input
              type="checkbox"
              className="row-checkbox"
              checked={allChecked}
              ref={(el) => {
                if (el) el.indeterminate = someChecked;
              }}
              onChange={onToggleAll}
            />
          </th>
          <th>Name</th>
          <th>Size</th>
          <th>Type</th>
          <th>Stored</th>
          <th>Savings</th>
          <th>Modified</th>
        </tr>
      </thead>
      <tbody>
        {folders.map((folder) => (
          <tr key={`folder:${folder}`} className="folder-row">
            <td />
            <td colSpan={6}>
              <span className="folder-name" onClick={() => onNavigate(folder)}>
                <span className="folder-icon">&#128193;</span>
                {displayName(folder, prefix)}
              </span>
            </td>
          </tr>
        ))}
        {objects.map((obj) => {
          const savings = savingsPercent(obj);
          const isChecked = selectedKeys.has(obj.key);
          return (
            <tr
              key={obj.key}
              className={`${selected?.key === obj.key ? 'selected' : ''} ${isChecked ? 'checked' : ''}`}
            >
              <td>
                <input
                  type="checkbox"
                  className="row-checkbox"
                  checked={isChecked}
                  onChange={() => onToggleKey(obj.key)}
                />
              </td>
              <td>
                <span className="file-name" onClick={() => onSelect(obj)}>
                  {displayName(obj.key, prefix)}
                </span>
              </td>
              <td>{formatBytes(obj.size)}</td>
              <td>
                {obj.storageType && (
                  <span className={badgeClass(obj.storageType)}>
                    {obj.storageType}
                  </span>
                )}
              </td>
              <td>{obj.storedSize != null ? formatBytes(obj.storedSize) : '--'}</td>
              <td>
                {savings != null ? (
                  <div className="savings-bar">
                    <div className="savings-bar-track">
                      <div
                        className="savings-bar-fill"
                        style={{ width: `${savings}%` }}
                      />
                    </div>
                    <span className="savings-pct">{savings.toFixed(0)}%</span>
                  </div>
                ) : (
                  <span className="savings-pct none">--</span>
                )}
              </td>
              <td style={{ fontSize: 12, color: 'var(--text-dim)' }}>
                {obj.lastModified
                  ? new Date(obj.lastModified).toLocaleString()
                  : '--'}
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}
