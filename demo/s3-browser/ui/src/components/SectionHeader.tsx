import { Typography } from 'antd';
import { useColors } from '../ThemeContext';

const { Text } = Typography;

export default function SectionHeader({ icon, title, description }: { icon: React.ReactNode; title: React.ReactNode; description?: string }) {
  const { ACCENT_BLUE, TEXT_PRIMARY, TEXT_MUTED } = useColors();
  return (
    <div style={{ marginBottom: 2 }}>
      <div style={{ display: 'flex', alignItems: 'center', gap: 8 }}>
        <div style={{
          width: 24,
          height: 24,
          borderRadius: 6,
          background: `linear-gradient(135deg, ${ACCENT_BLUE}18, ${ACCENT_BLUE}08)`,
          border: `1px solid ${ACCENT_BLUE}22`,
          display: 'flex',
          alignItems: 'center',
          justifyContent: 'center',
          flexShrink: 0,
          fontSize: 12,
          color: ACCENT_BLUE,
        }}>
          {icon}
        </div>
        <Text strong style={{ fontFamily: "var(--font-ui)", fontSize: 14, color: TEXT_PRIMARY, lineHeight: 1.2 }}>{title}</Text>
      </div>
      {description && (
        <Text style={{ fontFamily: "var(--font-ui)", fontSize: 12, color: TEXT_MUTED, display: 'block', marginTop: 2, marginLeft: 32, lineHeight: 1.35 }}>{description}</Text>
      )}
    </div>
  );
}
