import { Button, Modal } from 'antd';
import { type ConflictChoice, type TransferConflicts, conflictConfirm } from '../bulkConflicts';

/**
 * THE question of a bulk copy or move onto existing objects: skip them,
 * overwrite them, or cancel. Escape and the close icon answer "cancel".
 * The words come from `conflictConfirm` (pure, tested).
 */
export function confirmConflicts(c: TransferConflicts): Promise<ConflictChoice> {
  const { title, body } = conflictConfirm(c);
  return new Promise((resolve) => {
    const answer = (choice: ConflictChoice) => {
      resolve(choice);
      dialog.destroy();
    };
    const dialog = Modal.confirm({
      title,
      content: body.map((text, i) => (
        <p key={i} style={{ margin: i ? '8px 0 0' : 0 }}>
          {text}
        </p>
      )),
      closable: true,
      keyboard: true,
      onCancel: () => resolve('cancel'),
      footer: (
        <div style={{ display: 'flex', justifyContent: 'flex-end', gap: 8, flexWrap: 'wrap' }}>
          <Button onClick={() => answer('cancel')}>Cancel</Button>
          <Button danger onClick={() => answer('overwrite')}>
            Overwrite
          </Button>
          <Button type="primary" onClick={() => answer('skip')}>
            Skip existing
          </Button>
        </div>
      ),
    });
  });
}
