import { request } from '@umijs/max';

import { policyPoolPair, readEditablePolicy } from '@/domain/tieringPolicy';
import type { PolicyRevision } from './policies';

export interface TieringPolicy {
  id: number;
  revision: number;
  name: string;
  enabled: boolean;
  editable: boolean;
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
  previewAvailable: boolean;
  reason: string | null;
}

function toTieringPolicy({ id, revision, definition }: PolicyRevision): TieringPolicy[] {
  const pair = policyPoolPair(definition);
  if (!pair) return [];
  return [{
    id, revision, ...pair, name: definition.metadata.name,
    enabled: definition.spec.enabled ?? true,
    editable: readEditablePolicy(definition) !== null,
    filesystem: definition.spec.target.filesystem,
  }];
}

export async function listTieringPolicies(signal: AbortSignal) {
  const rows = await request<PolicyRevision[]>('/api/policies', {
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
