import { CopyOutlined } from '@ant-design/icons';
import type { RowAction } from '../RowActionsMenu';
import type { JobRow } from '../../jobsView';

type Rule = Pick<JobRow, 'id' | 'kind' | 'name'>;

/**
 * The "⋯" item that copies a replication or lifecycle rule into a new draft,
 * to point it at other buckets. It changes nothing until the operator applies.
 */
export function cloneRuleAction(rule: Rule, clone: (r: Rule) => void): RowAction {
  return {
    key: 'clone',
    icon: <CopyOutlined />,
    label: 'Clone',
    onSelect: () => clone(rule),
  };
}
