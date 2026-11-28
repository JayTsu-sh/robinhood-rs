import { request } from '@umijs/max';
import type { PolicyDefinition } from '@/domain/tieringPolicy';
import type { LatestRun } from './executions';
import type { PolicyPreview } from './policyPreview';
import { listFilesystems } from './tiering';

export interface PolicyRevision {
  id: number;
  revision: number;
  idExact: string;
  revisionExact: string;
  definition: PolicyDefinition;
  resourceBindings: PolicyPreview['resourceBindings'] | null;
  modifiedBy: string | null;
  latestRun?: LatestRun;
}

export interface ResourceRevision {
  name: string;
  revision: number;
  definition?: { spec: Record<string, unknown> };
}

const readOptions = { timeout: 15_000, skipErrorHandler: true };

export const policyDetailsPath = (id: number | string, revision?: number | string) =>
  revision === undefined ? `/policies/${encodeURIComponent(id)}` :
    `/policies/${encodeURIComponent(id)}/revisions/${encodeURIComponent(revision)}`;

export const policyEditPath = (id: number | string) => `${policyDetailsPath(id)}/edit`;

export function getPolicy(id: number | string, signal: AbortSignal, revision?: number | string, includeLatestRun = false) {
  return request<PolicyRevision>(`/api${policyDetailsPath(id, revision)}`, {
    ...readOptions, signal, params: includeLatestRun && revision === undefined ? { includeLatestRun: true } : undefined,
  });
}

export async function getPolicyDefinition(id: number | string, revision: number | string, signal: AbortSignal) {
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

export interface SavedRevision { id: string | number; revision: string | number }

export function writerTransportAllowed() {
  const { protocol, hostname } = window.location;
  return protocol === 'https:' || (protocol === 'http:' &&
    (hostname === 'localhost' || hostname === '[::1]' || /^127(?:\.\d{1,3}){3}$/.test(hostname)));
}

function policyWriteHeaders(base?: SavedRevision, token?: string) {
  if (token && !writerTransportAllowed()) {
    throw new Error('请通过本机回环地址或 HTTPS 连接后提交策略写入请求。');
  }
  const headers: Record<string, string> = {};
  if (base) headers['If-Match'] = `"${base.revision}"`;
  if (token) headers.Authorization = `Bearer ${token}`;
  return headers;
}

export function savePolicy(document: object, base?: SavedRevision, token?: string) {
  return request<Pick<PolicyRevision, 'idExact' | 'revisionExact'>>(base ? `/api${policyDetailsPath(base.id)}` : '/api/policies', {
    method: base ? 'PUT' : 'POST', data: document, headers: policyWriteHeaders(base, token), timeout: 60_000,
    skipErrorHandler: true,
  });
}

export function setPolicyEnabled(base: SavedRevision, enabled: boolean, token?: string, signal?: AbortSignal) {
  return request<Pick<PolicyRevision, 'idExact' | 'revisionExact'>>(`/api${policyDetailsPath(base.id)}`, {
    method: 'PATCH', data: { enabled }, headers: policyWriteHeaders(base, token), signal, timeout: 60_000,
    skipErrorHandler: true,
  });
}

export function requestPolicyRun(base: SavedRevision, token?: string, signal?: AbortSignal) {
  return request<{ runIdExact: string }>(`/api${policyDetailsPath(base.id)}/run`, {
    method: 'POST', headers: policyWriteHeaders(base, token), signal, timeout: 60_000, skipErrorHandler: true,
  });
}

export function writeError(error: unknown) {
  return (error as {
    response?: { status?: number; data?: {
      currentRevisionExact?: string; activeRunIdExact?: string; existingRunIdExact?: string; field?: string;
    } };
  } | undefined)?.response;
}
