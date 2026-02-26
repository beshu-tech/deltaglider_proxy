import { Typography } from 'antd';
import { useColors } from '../ThemeContext';

const { Text } = Typography;

export default function SectionHeader({ icon, title }: { icon: React.ReactNode; title: string }) {
  const { ACCENT_BLUE, TEXT_PRIMARY } = useColors();
  return (
    <div style={{ display: 'flex', alignItems: 'center', gap: 10, marginBottom: 4 }}>
      <div style={{
        width: 28,
        height: 28,
        borderRadius: 7,
        background: `linear-gradient(135deg, ${ACCENT_BLUE}18, ${ACCENT_BLUE}08)`,
        border: `1px solid ${ACCENT_BLUE}22`,
        display: 'flex',
        alignItems: 'center',
        justifyContent: 'center',
        flexShrink: 0,
        fontSize: 14,
        color: ACCENT_BLUE,
      }}>
        {icon}
      </div>
      <Text strong style={{ fontFamily: "var(--font-ui)", fontSize: 15, color: TEXT_PRIMARY }}>{title}</Text>
    </div>
  );
}
