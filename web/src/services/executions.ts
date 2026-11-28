import { request } from '@umijs/max';

export interface LatestRun {
  id: string;
  revision: string;
  state: string;
  startedAt: string | null;
}

export interface RunHistory {
  idExact: string;
  revisionExact: string;
  policyIdExact: string;
  state: string;
  startedAt: string | null;
  policyName: string | null;
  activationMode: string | null;
  policyMode: string | null;
  reason: string | null;
  candidateCount: number | null;
  estimatedBytesExact: string | null;
  requestedBy: string | null;
  createdAt: string;
}

export interface RunDetail extends RunHistory {
  hasPendingOperations: boolean;
}

export interface FileOperation {
  idExact: string;
  filesystem: string;
  fid: string;
  action: string;
  sizeExact: string;
  state: string;
  reason: string | null;
  attempted: boolean;
  acceptedAt: string | null;
  finishedAt: string | null;
  lastCheckedAt: string | null;
  migrationPlan?: { sourcePool: string; targetPool: string; stripeCount: number; stripeSize: number };
  completionEvidence: Record<string, unknown> | null;
}

export interface CursorPage<T> {
  items: T[];
  nextCursor: string | null;
}

export interface RunFilters {
  policyId?: string;
  policyName?: string;
  runId?: string;
  state?: string;
}

export interface OperationFilters {
  state?: string;
  reason?: string;
  fid?: string;
  exceptionsOnly?: true;
}

const readOptions = { timeout: 15_000, skipErrorHandler: true };

export function listRuns(filters: RunFilters, beforeId: string | undefined, signal: AbortSignal) {
  return request<CursorPage<RunHistory>>('/api/runs', {
    ...readOptions, params: { ...filters, beforeId }, signal,
  });
}

export function getRun(id: string, signal: AbortSignal) {
  return request<RunDetail>(`/api/runs/${encodeURIComponent(id)}`, { ...readOptions, signal });
}

export function listOperations(id: string, filters: OperationFilters, afterId: string | undefined, signal: AbortSignal) {
  return request<CursorPage<FileOperation>>(`/api/runs/${encodeURIComponent(id)}/operations`, {
    ...readOptions, params: { ...filters, afterId }, signal,
  });
}

export const runDetailsPath = (id: string) => `/runs/${id}`;

export function runHistoryPath(policyId: string) {
  return `/runs?${new URLSearchParams({ policyId: String(policyId) })}`;
}

export function executionFilters(params: URLSearchParams, values: Record<string, string | undefined>) {
  const next = new URLSearchParams(params);
  for (const [key, value] of Object.entries(values)) {
    if (value) next.set(key, value);
    else next.delete(key);
  }
  return next;
}
