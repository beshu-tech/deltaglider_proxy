/**
 * System — ONE page for the platform/runtime knobs that used to be five
 * separate Advanced screens plus Backup: Listener & TLS, Caches, runtime
 * Limits (read-only env vars), Logging, Config DB sync, the encryption
 * witness (several instances only), Configuration as YAML and Backup.
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
import { useQuery } from '@tanstack/react-query';
import RecoveryPanel from './RecoveryPanel';
import ConfigYamlCard, { type ConfigYamlActions } from './ConfigYamlCard';
import { CARD_GAP, contentColumn, CONTENT_FORM } from './shared-styles';
import EncryptionWitnessCard from './EncryptionWitnessCard';
import { ErrorState, LoadingState } from './StatePlaceholders';
import { getSectionVersioned } from '../adminApi';
import { qk } from '../queries/keys';
import { isSessionExpired } from '../errorHandling';
import { useSessionExpiredOn } from '../hooks/useSessionExpiredOn';

interface Props {
  onSessionExpired?: () => void;
  onExportBackup: () => void;
  onImportBackup: () => void;
  configYaml: ConfigYamlActions;
}

export default function SystemPanel({ onSessionExpired, onExportBackup, onImportBackup, configYaml }: Props) {
  // ONE load gate for the page (UX harvest D16, D17): while the settings
  // load, card skeletons; when they cannot load, ONE error with Retry, not
  // one "Failed to load" box per card (and no card that exists only in the
  // error state). The cards then read their own subsets.
  const gate = useQuery({
    queryKey: qk.sectionGate('advanced'),
    queryFn: () => getSectionVersioned('advanced'),
    retry: false,
  });
  useSessionExpiredOn(gate.error, onSessionExpired);
  const column = { ...contentColumn(CONTENT_FORM), display: 'flex', flexDirection: 'column', gap: CARD_GAP } as const;
  if (gate.isPending) {
    return (
      <div style={column}>
        <LoadingState variant="card" rows={2} label="Loading the system settings" />
        <LoadingState variant="card" rows={4} label="Loading the system settings" />
        <LoadingState variant="card" rows={3} label="Loading the system settings" />
      </div>
    );
  }
  if (gate.isError) {
    if (isSessionExpired(gate.error)) return null;
    return (
      <div style={column}>
        <ErrorState error={gate.error} what="the system settings" onRetry={() => void gate.refetch()} />
      </div>
    );
  }
  // ONE column and ONE gap for every card on the page (the cards add no
  // margin or padding of their own).
  return (
    <div style={column}>
      <ListenerTlsPanel onSessionExpired={onSessionExpired} />
      <CachesPanel onSessionExpired={onSessionExpired} />
      <LimitsPanel onSessionExpired={onSessionExpired} />
      <LoggingPanel onSessionExpired={onSessionExpired} />
      <ConfigDbSyncPanel onSessionExpired={onSessionExpired} />
      <EncryptionWitnessCard />
      <ConfigYamlCard {...configYaml} />
      <RecoveryPanel onExportBackup={onExportBackup} onImportBackup={onImportBackup} />
    </div>
  );
}
