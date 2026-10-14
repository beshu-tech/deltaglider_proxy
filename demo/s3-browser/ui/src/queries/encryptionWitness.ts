/**
 * Encryption witness queries (issue #116 P3). The verdict is `null` on a
 * single instance. Each instance re-reads the published settings every
 * 30 s (DGP_ENCRYPTION_WITNESS_POLL_SECS), so the view polls at half that.
 */
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { getEncryptionWitness, publishEncryptionWitness } from '../adminApi';
import { qk } from './keys';

const WITNESS_REFRESH_MS = 15_000;

export function useEncryptionWitness() {
  return useQuery({
    queryKey: qk.encryptionWitness(),
    queryFn: getEncryptionWitness,
    refetchInterval: WITNESS_REFRESH_MS,
  });
}

export function usePublishEncryptionWitness() {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: ({ storages, downgrades }: { storages: string[]; downgrades: string[] }) =>
      publishEncryptionWitness(storages, downgrades),
    onSettled: () => qc.invalidateQueries({ queryKey: qk.encryptionWitness() }),
  });
}
