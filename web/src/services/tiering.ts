import { request } from '@umijs/max';

import { policyPoolPair, readEditablePolicy } from '@/domain/tieringPolicy';
import type { LatestRun } from './executions';
import type { PolicyRevision } from './policies';

export interface TieringPolicy {
  id: string;
  revision: string;
  name: string;
  enabled: boolean;
  editable: boolean;
  filesystem: string;
  sourcePool: string;
  targetPool: string;
  latestRun?: LatestRun;
}

export interface PoolResource {
  name: string;
  ostIndices: number[] | null;
  eligible: boolean;
  reason: string | null;
  capacity: Capacity;
}

export interface Capacity {
  status: 'collected' | 'partial' | 'missing' | 'error';
  attemptedAt: number | null;
  sample: {
    observedAt: number;
    totalBytes: number;
    usedBytes: number;
    availableBytes: number;
    usedRatio: number;
  } | null;
  reason: string | null;
}

export interface FilesystemResource {
  id: string;
  mountPath: string;
  mountStatus: 'mounted' | 'unmounted' | 'unknown';
  collectionStatus: 'pending' | 'collected' | 'partial' | 'unavailable' | 'error';
  observedAt: number | null;
  mount: { source: string; readOnly: boolean } | null;
  capacity: Capacity;
  pools: PoolResource[] | null;
  ostIndices?: number[] | null;
  mdtNames?: string[] | null;
  tieringAvailable: boolean;
  previewAvailable: boolean;
  reason: string | null;
}

function toTieringPolicy({ idExact, revisionExact, definition, latestRun }: PolicyRevision): TieringPolicy[] {
  const pair = policyPoolPair(definition);
  if (!pair) return [];
  return [{
    id: idExact, revision: revisionExact, ...pair, name: definition.metadata.name,
    enabled: definition.spec.enabled ?? true,
    editable: readEditablePolicy(definition) !== null,
    filesystem: definition.spec.target.filesystem, latestRun,
  }];
}

export async function listTieringPolicies(signal: AbortSignal, includeLatestRun = false) {
  const rows = await request<PolicyRevision[]>('/api/policies', {
    params: includeLatestRun ? { configuredOnly: true, includeLatestRun } : { configuredOnly: true },
    signal,
    timeout: 15000,
    skipErrorHandler: true,
  });
  return rows.flatMap(toTieringPolicy);
}

export function listFilesystems(signal: AbortSignal) {
  return request<FilesystemResource[]>('/api/filesystems', { signal, timeout: 15000, skipErrorHandler: true });
}

export async function getFilesystem(id: string, signal: AbortSignal) {
  try {
    return await request<FilesystemResource>('/api/filesystems/detail', {
      params: { filesystem: id }, signal, timeout: 15000, skipErrorHandler: true,
    });
  } catch (error: unknown) {
    if ((error as { response?: { status?: number } } | null)?.response?.status === 404) return null;
    throw error;
  }
}

export function filesystemDetailsPath(id: string) {
  return `/resources/detail?${new URLSearchParams({ filesystem: id })}`;
}
