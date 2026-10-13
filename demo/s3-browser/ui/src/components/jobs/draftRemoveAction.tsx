import { DeleteOutlined } from '@ant-design/icons';
import type { RowAction } from '../RowActionsMenu';
import type { JobRow } from '../../jobsView';

type Draft = Pick<JobRow, 'id' | 'kind' | 'name'>;

/**
 * The "⋯" item that drops a draft: a rule that exists only in the unsaved
 * edits. It touches no server state, and the other unsaved edits stay.
 */
export function draftRemoveAction(draft: Draft, remove: (d: Draft) => void): RowAction {
  return {
    key: 'remove-draft',
    danger: true,
    icon: <DeleteOutlined />,
    label: 'Remove draft…',
    confirm: {
      title: `Remove the draft ${draft.name}?`,
      content: 'The rule was never applied. Your other unsaved edits stay.',
      okText: 'Remove draft',
    },
    onSelect: () => remove(draft),
  };
}
