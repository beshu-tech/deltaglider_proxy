import type { CSSProperties } from 'react';
import { Alert } from 'antd';
import type { PlainError } from '../adminApi';

/**
 * An error Alert for a `PlainError` (see `plainError` in adminApi/core):
 * the plain sentence as the title, and the raw server text folded away
 * under "Details" for a support request or a log search.
 */
export default function PlainErrorAlert({ error, style }: { error: PlainError; style?: CSSProperties }) {
  return (
    <Alert
      type="error"
      showIcon
      title={error.text}
      description={
        error.details ? (
          <details>
            <summary style={{ cursor: 'pointer', fontSize: 12 }}>Details</summary>
            <code style={{ display: 'block', marginTop: 4, fontSize: 11, overflowWrap: 'anywhere', whiteSpace: 'pre-wrap' }}>
              {error.details}
            </code>
          </details>
        ) : undefined
      }
      style={{ borderRadius: 8, ...style }}
    />
  );
}
