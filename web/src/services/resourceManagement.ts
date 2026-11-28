import { request } from '@umijs/max';
import { policyWriteHeaders } from './policies';
import type { FilesystemResource } from './tiering';

export type PoolTier = 'hot' | 'warm' | 'cold';

export interface ResourceRegistration {
  id: string;
  name: string;
  owner: 'deployment' | 'external' | 'daemon';
  path: string;
  source: string;
  options: string[];
  autoMount: boolean;
  desiredMounted: boolean;
  mounted?: boolean;
  lastMountError?: string;
  tiers: Record<string, PoolTier>;
}

interface ResourceManagement {
  canCreateMount: boolean;
  resources: ResourceRegistration[];
  availableMounts: { path: string; source: string }[];
}

export const poolTiers = { hot: '热层', warm: '温层', cold: '冷层' } as const;
export const resourceOwnerLabels: Record<ResourceRegistration['owner'], string> = {
  deployment: '部署管理', external: '外部管理', daemon: '应用管理',
};

export function withLiveMountStatus(resource: FilesystemResource, registration?: ResourceRegistration): FilesystemResource {
  if (registration?.mounted === undefined || registration.path !== resource.mountPath) return resource;
  const mountStatus = registration.mounted ? 'mounted' : 'unmounted';
  if (mountStatus === resource.mountStatus) return resource;
  // Mount info can confirm the mount even when the collection attempt timed out.
  if (registration.mounted && resource.mountStatus === 'unknown') return { ...resource, mountStatus };

  const reason = registration.mounted ? 'collection_pending' : 'not_a_lustre_mount';
  return {
    ...resource,
    mountStatus,
    collectionStatus: registration.mounted ? 'pending' : 'unavailable',
    observedAt: null,
    mount: null,
    capacity: { status: 'missing', attemptedAt: null, sample: null, reason: null },
    pools: null,
    ostIndices: null,
    mdtNames: null,
    tieringAvailable: false,
    previewAvailable: false,
    reason,
  };
}

const endpoint = '/api/filesystems/management';

export function listResourceManagement(signal: AbortSignal) {
  return request<ResourceManagement>(endpoint, { signal, timeout: 15_000, skipErrorHandler: true });
}

export function writeResource(
  path: string, data: object, signal: AbortSignal, token?: string,
) {
  return request<ResourceRegistration | undefined>(endpoint + path, {
    method: 'POST', data, signal, headers: policyWriteHeaders(undefined, token),
    timeout: 75_000, skipErrorHandler: true,
  });
}
