import type { PolicySettings, WizardValues } from '@/domain/tieringPolicy';
import type { SavedRevision } from './policies';

export interface PolicyDraft {
  version: 1;
  values: WizardValues;
  base?: SavedRevision;
  settings: PolicySettings;
}

function draftKey(id?: number) {
  return `rbh.console.policy-draft.v1.${id ?? 'new'}`;
}

export function readPolicyDraft(id?: number): PolicyDraft | null {
  try {
    const draft = JSON.parse(localStorage.getItem(draftKey(id)) ?? 'null') as PolicyDraft | null;
    if (draft?.version !== 1 || draft.base?.id !== id || !draft.values || !draft.settings) return null;
    if (typeof draft.values.name !== 'string' || !Array.isArray(draft.values.rules) ||
      !Array.isArray(draft.values.include?.clauses) || !Array.isArray(draft.values.exclude?.clauses) ||
      !Array.isArray(draft.values.excludedSubtrees)) return null;
    return draft;
  } catch { return null; }
}

export function savePolicyDraft(draft: PolicyDraft) {
  localStorage.setItem(draftKey(draft.base?.id), JSON.stringify(draft));
}

export function removePolicyDraft(draft: PolicyDraft) {
  const key = draftKey(draft.base?.id);
  // Preserve a draft saved by another tab since this form was opened.
  if (localStorage.getItem(key) === JSON.stringify(draft)) localStorage.removeItem(key);
}
