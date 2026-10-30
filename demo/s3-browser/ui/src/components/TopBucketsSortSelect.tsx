import { Select } from 'antd';
import { SORT_LABELS, type TopBucketsSortKey } from './topBucketsSort';

const OPTIONS = (Object.keys(SORT_LABELS) as TopBucketsSortKey[]).map(k => ({
  value: k,
  label: `Sort: ${SORT_LABELS[k]}`,
}));

/** Sort control in the Buckets panel header: the same AntD Select as every other dropdown. */
export default function TopBucketsSortSelect({
  value,
  onChange,
}: {
  value: TopBucketsSortKey;
  onChange: (v: TopBucketsSortKey) => void;
}) {
  return (
    <Select<TopBucketsSortKey>
      size="small"
      value={value}
      onChange={onChange}
      options={OPTIONS}
      aria-label="Sort buckets"
      popupMatchSelectWidth={false}
      style={{ minWidth: 170 }}
    />
  );
}
