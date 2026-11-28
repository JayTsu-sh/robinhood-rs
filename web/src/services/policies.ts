import { request } from '@umijs/max';
import type { PolicyDefinition } from '@/domain/tieringPolicy';
import type { PolicyPreview } from './policyPreview';
import { listFilesystems } from './tiering';

export interface PolicyRevision {
  id: number;
  revision: number;
  definition: PolicyDefinition;
  resourceBindings: PolicyPreview['resourceBindings'] | null;
  modifiedBy: string | null;
}

export interface ResourceRevision {
  name: string;
  revision: number;
  definition?: { spec: Record<string, unknown> };
}

const readOptions = { timeout: 15_000, skipErrorHandler: true };

export const policyDetailsPath = (id: number, revision?: number) =>
  revision === undefined ? `/policies/${id}` : `/policies/${id}/revisions/${revision}`;

export const policyEditPath = (id: number) => `${policyDetailsPath(id)}/edit`;

export function getPolicy(id: number, signal: AbortSignal, revision?: number) {
  return request<PolicyRevision>(`/api${policyDetailsPath(id, revision)}`, { ...readOptions, signal });
}

export async function getPolicyDefinition(id: number, revision: number, signal: AbortSignal) {
  const response = await fetch(`/api${policyDetailsPath(id, revision)}/definition`, {
    signal: AbortSignal.any([signal, AbortSignal.timeout(readOptions.timeout)]),
  });
  if (!response.ok) throw new Error(`HTTP ${response.status}`);
  return response.text();
}

export async function loadWizardResources(signal: AbortSignal) {
  const list = (path: string) => request<ResourceRevision[]>(`/api/${path}`, { ...readOptions, signal });
  const [filesystems, profile, authorization, approval] = await Promise.all([
    listFilesystems(signal), list('execution-profiles'), list('authorization-policies'),
    list('approval-policies'),
  ]);
  return { filesystems, profile, authorization, approval };
}

export type WizardResources = Awaited<ReturnType<typeof loadWizardResources>>;

export interface SavedRevision { id: number; revision: number }

export function writerTransportAllowed() {
  const { protocol, hostname } = window.location;
  return protocol === 'https:' || (protocol === 'http:' &&
    (hostname === 'localhost' || hostname === '[::1]' || /^127(?:\.\d{1,3}){3}$/.test(hostname)));
}

export function savePolicy(document: object, base?: SavedRevision, token?: string) {
  if (token && !writerTransportAllowed()) {
    throw new Error('请通过本机回环地址或 HTTPS 连接后使用 Rev.8 writer。');
  }
  const headers: Record<string, string> = {};
  if (base) headers['If-Match'] = `"${base.revision}"`;
  if (token) headers.Authorization = `Bearer ${token}`;
  return request<SavedRevision>(base ? `/api/policies/${base.id}` : '/api/policies', {
    method: base ? 'PUT' : 'POST', data: document, headers, timeout: 60_000, skipErrorHandler: true,
  });
}

export function writeError(error: unknown) {
  return (error as {
    response?: { status?: number; data?: { currentRevision?: number; field?: string } };
  } | undefined)?.response;
}
