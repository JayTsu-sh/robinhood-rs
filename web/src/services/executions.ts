import { request } from '@umijs/max';
import type { DirectoryScopeSummary } from './policyPreview';
import { policyWriteHeaders } from './policies';

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
  finishedAt?: string | null;
  policyName: string | null;
  activationMode: string | null;
  policyMode: string | null;
  reason: string | null;
  candidateCount: number | null;
  estimatedBytesExact: string | null;
  requestedBy: string | null;
  approvedBy?: string | null;
  createdAt: string;
  operationResults?: OperationResults;
}

export interface RunDetail extends RunHistory {
  hasPendingOperations: boolean;
  directoryScope?: DirectoryScopeSummary;
}

export interface OperationResults {
  matchedCount: number;
  pendingCount: number;
  completedLogicalBytes: string;
  byState: Record<string, number>;
}

export interface OperationSummary extends OperationResults {
  byReason: { reason: string | null; count: number; byState: Record<string, number> }[];
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
  attemptCount?: number | null;
  paths?: string[] | null;
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
  activationMode?: string;
}

export interface OperationFilters {
  state?: string;
  reason?: string;
  fid?: string;
  path?: string;
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

export interface RunStatistics {
  total: number;
  queued: number;
  running: number;
  paused: number;
  awaitingApproval: number;
  ended: number;
  exceptionRuns: number;
  completedObjects: number;
  failedObjects: number;
  completedLastDay: number;
  completedLogicalBytes: string;
}

export function getRunStatistics(signal: AbortSignal, policyId?: string) {
  return request<RunStatistics>('/api/runs/summary', { ...readOptions, params: { policyId }, signal });
}

interface RunTelemetry {
  controls: { action: string; actor: string; occurredAtMillis: number }[];
  completions: { minute: number; logicalBytes: string }[];
}

export function getRunTelemetry(id: string, signal: AbortSignal) {
  return request<RunTelemetry>(`/api/runs/${encodeURIComponent(id)}/telemetry`, { ...readOptions, signal });
}

export type RunControl = 'pause' | 'resume' | 'cancel' | 'approve';

export function controlRun(id: string, action: RunControl, signal: AbortSignal, token?: string) {
  return request(`/api/runs/${encodeURIComponent(id)}/control`, {
    method: 'POST', data: { action }, headers: policyWriteHeaders(undefined, token),
    timeout: 60_000, signal, skipErrorHandler: true,
  });
}

export function listOperations(id: string, filters: OperationFilters, afterId: string | undefined, signal: AbortSignal) {
  return request<CursorPage<FileOperation>>(`/api/runs/${encodeURIComponent(id)}/operations`, {
    ...readOptions, params: { ...filters, afterId }, signal,
  });
}

export function summarizeOperations(id: string, filters: OperationFilters, signal: AbortSignal) {
  return request<OperationSummary>(`/api/runs/${encodeURIComponent(id)}/operations/summary`, {
    ...readOptions, params: filters, signal,
  });
}

export function runReportUrl(id: string, { state, reason, fid, path: filePath, exceptionsOnly }: OperationFilters) {
  const params = new URLSearchParams();
  if (state) params.set('state', state);
  if (reason) params.set('reason', reason);
  if (fid) params.set('fid', fid);
  if (filePath) params.set('path', filePath);
  const path = `/api/runs/${encodeURIComponent(id)}/reports/${exceptionsOnly ? 'exceptions' : 'run'}`;
  return params.size ? `${path}?${params}` : path;
}

export function operationStateFilterKey(exceptionsOnly?: true) {
  return exceptionsOnly ? 'exceptionState' : 'operationState';
}

export function operationFilters(params: URLSearchParams, exceptionsOnly?: true): OperationFilters {
  return {
    fid: params.get('fid') ?? undefined,
    path: params.get('path') ?? undefined,
    reason: params.get('reason') ?? undefined,
    state: params.get(operationStateFilterKey(exceptionsOnly)) ?? undefined,
    exceptionsOnly,
  };
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
