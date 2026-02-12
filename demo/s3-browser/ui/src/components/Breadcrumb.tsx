import { Breadcrumb as AntBreadcrumb } from 'antd';
import { DatabaseOutlined } from '@ant-design/icons';
import { prefixSegments } from '../utils';
import { getBucket } from '../s3client';

interface Props {
  prefix: string;
  onNavigate: (prefix: string) => void;
}

export default function Breadcrumb({ prefix, onNavigate }: Props) {
  const segments = prefixSegments(prefix);

  const items = [
    {
      title: (
        <span
          onClick={() => prefix && onNavigate('')}
          style={{ cursor: prefix ? 'pointer' : 'default' }}
        >
          <DatabaseOutlined style={{ marginRight: 6 }} />
          {getBucket()}
        </span>
      ),
    },
    ...segments.map((seg) => ({
      title: (
        <span
          onClick={() => seg.prefix !== prefix && onNavigate(seg.prefix)}
          style={{ cursor: seg.prefix !== prefix ? 'pointer' : 'default' }}
        >
          {seg.label}
        </span>
      ),
    })),
  ];

  return <AntBreadcrumb items={items} />;
}
