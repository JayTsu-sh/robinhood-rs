import { request } from '@umijs/max';

interface PolicyAction {
  type?: string;
  params?: { sourcePool?: string; targetPool?: string };
}

interface Rev8Policy {
  id: number;
  revision: number;
  definition: {
    metadata: { name: string };
    spec: {
      enabled?: boolean;
      target: { filesystem: string };
      rules: {
        defaults?: { action?: PolicyAction };
        entries: { decision: 'execute' | 'require_approval' | 'skip'; action?: PolicyAction }[];
      };
    };
  };
}

export interface TieringPolicy {
  id: number;
  revision: number;
  name: string;
  enabled: boolean;
  filesystem: string;
  sourcePool: string;
  targetPool: string;
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
  tieringAvailable: boolean;
  reason: string | null;
}

const poolMigration = 'lustre.pool_migrate';

function toTieringPolicy({ id, revision, definition }: Rev8Policy): TieringPolicy[] {
  const { metadata, spec } = definition;
  const defaults = spec.rules.defaults?.action;
  const rule = spec.rules.entries.find(
    ({ decision, action }) =>
      decision !== 'skip' && (action?.type ?? defaults?.type) === poolMigration,
  );
  if (!rule) return [];

  // Rev.8 requires all migration rules to resolve to the same pool pair.
  const defaultParams = defaults?.type === poolMigration ? defaults.params : undefined;
  const { sourcePool, targetPool } = { ...defaultParams, ...rule.action?.params };
  if (!sourcePool || !targetPool) throw new Error(`策略 ${id} 缺少池对参数`);

  return [{
    id,
    revision,
    name: metadata.name,
    enabled: spec.enabled ?? true,
    filesystem: spec.target.filesystem,
    sourcePool,
    targetPool,
  }];
}

export async function listTieringPolicies(signal: AbortSignal) {
  const rows = await request<Rev8Policy[]>('/api/policies', {
    params: { configuredOnly: true },
    signal,
    timeout: 15000,
    skipErrorHandler: true,
  });
  return rows.flatMap(toTieringPolicy);
}

export function listFilesystems(signal: AbortSignal) {
  return request<FilesystemResource[]>('/api/filesystems', { signal, timeout: 15000, skipErrorHandler: true });
}

export function getFilesystem(id: string, signal: AbortSignal) {
  return request<FilesystemResource>('/api/filesystems/detail', {
    params: { filesystem: id }, signal, timeout: 15000, skipErrorHandler: true,
  });
}

function collectFilesystems(signal: AbortSignal, id?: string) {
  return request('/api/filesystems/refresh', {
    method: 'POST', params: id === undefined ? undefined : { filesystem: id },
    signal, timeout: 25000, skipErrorHandler: true,
  });
}

export async function refreshFilesystems(signal: AbortSignal) {
  await collectFilesystems(signal);
  return listFilesystems(signal);
}

export async function refreshFilesystem(id: string, signal: AbortSignal) {
  await collectFilesystems(signal, id);
  return getFilesystem(id, signal);
}

export function filesystemDetailsPath(id: string) {
  return `/resources/detail?${new URLSearchParams({ filesystem: id })}`;
}
