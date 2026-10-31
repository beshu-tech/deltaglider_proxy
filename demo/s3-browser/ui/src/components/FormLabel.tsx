import { Typography } from 'antd';

const { Text } = Typography;

/**
 * Form-field label (sentence case) used across the IAM forms (UserForm, GroupForm).
 * Previously each form re-declared an identical local `label` helper.
 */
export default function FormLabel({ text, hint, htmlFor }: { text: string; hint?: string; htmlFor?: string }) {
  // `htmlFor` makes the text the control's accessible name (the hint stays out of it).
  return (
    <div style={{ marginBottom: 4 }}>
      <label htmlFor={htmlFor}>
        <Text type="secondary" style={{ fontSize: 12, fontWeight: 600 }}>{text}</Text>
      </label>
      {hint && <Text type="secondary" style={{ fontSize: 12, fontWeight: 400, marginLeft: 8 }}>{hint}</Text>}
    </div>
  );
}
