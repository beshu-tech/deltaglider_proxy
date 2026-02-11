import { prefixSegments } from '../utils';

interface Props {
  prefix: string;
  onNavigate: (prefix: string) => void;
}

export default function Breadcrumb({ prefix, onNavigate }: Props) {
  const segments = prefixSegments(prefix);

  return (
    <nav className="breadcrumb">
      <span
        className={`breadcrumb-item ${prefix === '' ? 'active' : 'clickable'}`}
        onClick={() => prefix && onNavigate('')}
      >
        &#128463; Root
      </span>
      {segments.map((seg) => (
        <span key={seg.prefix}>
          <span className="breadcrumb-sep">/</span>
          <span
            className={`breadcrumb-item ${seg.prefix === prefix ? 'active' : 'clickable'}`}
            onClick={() => seg.prefix !== prefix && onNavigate(seg.prefix)}
          >
            {seg.label}
          </span>
        </span>
      ))}
    </nav>
  );
}
