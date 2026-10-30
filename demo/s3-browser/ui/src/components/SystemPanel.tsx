/**
 * System — ONE page for the platform/runtime knobs that used to be five
 * separate Advanced screens plus Backup: Listener & TLS, Caches, runtime
 * Limits (read-only env vars), Logging, Config DB sync, the encryption
 * witness (several instances only), and Backup.
 *
 * Each card keeps its OWN section editor (independent dirty key + apply
 * dialog — the sidebar dot lights when any of them is dirty). The dirty
 * bars render INLINE inside their card so several can be visible at once
 * without overlapping.
 */
import {
  CachesPanel,
  ConfigDbSyncPanel,
  LimitsPanel,
  ListenerTlsPanel,
  LoggingPanel,
} from './advancedPanels';
import RecoveryPanel from './RecoveryPanel';
import { contentColumn, CONTENT_FORM } from './shared-styles';
import EncryptionWitnessCard from './EncryptionWitnessCard';

interface Props {
  onSessionExpired?: () => void;
  onExportBackup: () => void;
  onImportBackup: () => void;
}

export default function SystemPanel({ onSessionExpired, onExportBackup, onImportBackup }: Props) {
  // ONE column and ONE gap for every card on the page (the cards add no
  // margin or padding of their own).
  return (
    <div style={{ ...contentColumn(CONTENT_FORM), display: 'flex', flexDirection: 'column', gap: 24 }}>
      <ListenerTlsPanel onSessionExpired={onSessionExpired} />
      <CachesPanel onSessionExpired={onSessionExpired} />
      <LimitsPanel onSessionExpired={onSessionExpired} />
      <LoggingPanel onSessionExpired={onSessionExpired} />
      <ConfigDbSyncPanel onSessionExpired={onSessionExpired} />
      <EncryptionWitnessCard />
      <RecoveryPanel onExportBackup={onExportBackup} onImportBackup={onImportBackup} />
    </div>
  );
}
