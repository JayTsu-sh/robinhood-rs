import { request } from '@umijs/max';
import { policyWriteHeaders } from './policies';

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
  lastMountError?: string;
  tiers: Record<string, PoolTier>;
}

interface ResourceManagement {
  canCreateMount: boolean;
  resources: ResourceRegistration[];
  availableMounts: { path: string; source: string }[];
}

export const poolTiers = { hot: '热层', warm: '温层', cold: '冷层' } as const;

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
