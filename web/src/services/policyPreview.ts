import { request } from '@umijs/max';

export interface PreviewScope {
  filesystem: string;
  sourcePool: string;
  targetPool: string;
  root: string;
  excludedSubtrees?: string[];
}

export interface DirectoryScopeSummary {
  excludedByReason: Record<string, number>;
  deferredByReason: Record<string, number>;
}

export interface PolicyPreview {
  revision: number | null;
  candidateCount: number;
  estimatedBytes: number;
  estimatedBytesExact?: string;
  resourceBindings: { kind: string; name: string; revision: number }[];
  directoryScope?: DirectoryScopeSummary;
}

export function previewPolicyDocument(document: object, signal: AbortSignal) {
  return request<PolicyPreview>('/api/policies/preview', {
    method: 'POST', data: document, signal, timeout: 60_000, skipErrorHandler: true,
  });
}

export function poolScopePreviewDocument({
  filesystem, sourcePool, targetPool, root, excludedSubtrees = [],
}: PreviewScope) {
  // This preview covers the full selected source pool and directory scope.
  // The policy engine applies execution limits only to dispatch.
  const maxLogicalBytes = '9223372036854775807B';
  return {
    apiVersion: 'hsm.storage.io/v1alpha3',
    kind: 'Policy',
    metadata: { name: 'tiering-scope-preview' },
    spec: {
      enabled: false,
      target: {
        filesystem, objectType: 'file', directoryScope: { root, excludedSubtrees },
      },
      trigger: { type: 'periodic', interval: '1h' },
      objective: { type: 'exhaust' },
      rules: {
        entries: [{
          match: true,
          decision: 'execute',
          action: {
            type: 'lustre.pool_migrate',
            params: { sourcePool, targetPool, maxFileBytes: maxLogicalBytes },
          },
        }],
      },
      execution: {
        profileRef: 'builtin-preview',
        activation: { mode: 'preview' },
        budget: { run: { maxOperations: 1, maxSubmitAttempts: 1, maxObjectLogicalBytes: maxLogicalBytes } },
      },
      governance: { authorizationRef: 'builtin-preview', approvalPolicyRef: 'builtin-preview' },
    },
  };
}
