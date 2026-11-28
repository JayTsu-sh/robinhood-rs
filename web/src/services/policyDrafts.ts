import type { PolicySettings, WizardValues } from '@/domain/tieringPolicy';
import type { SavedRevision } from './policies';

export interface PolicyDraft {
  version: 1;
  values: WizardValues;
  base?: SavedRevision;
  settings: PolicySettings;
  slot?: string;
  savedAt?: number;
  confirmed?: number;
}

const prefix = 'rbh.console.policy-draft.v1.';

function draftKey(id?: SavedRevision['id'], slot?: string) {
  const base = String(id ?? 'new');
  return `${prefix}${base}${slot ? `.${slot}` : ''}`;
}

function decodeDraft(raw: string | null): PolicyDraft | null {
  try {
    const draft = JSON.parse(raw ?? 'null') as PolicyDraft | null;
    if (draft?.version !== 1 || !draft.values || !draft.settings) return null;
    if (typeof draft.values.name !== 'string' || !Array.isArray(draft.values.rules) ||
      !Array.isArray(draft.values.include?.clauses) || !Array.isArray(draft.values.exclude?.clauses) ||
      !Array.isArray(draft.values.excludedSubtrees)) return null;
    return draft;
  } catch { return null; }
}

export function listPolicyDrafts(): PolicyDraft[] {
  const result: PolicyDraft[] = [];
  try {
    for (let index = 0; index < localStorage.length; index += 1) {
      const key = localStorage.key(index);
      if (!key?.startsWith(prefix)) continue;
      const draft = decodeDraft(localStorage.getItem(key));
      if (draft && key === draftKey(draft.base?.id, draft.slot)) result.push(draft);
    }
  } catch { /* Storage can be disabled by the browser. */ }
  return result.sort((a, b) => (b.savedAt ?? 0) - (a.savedAt ?? 0));
}

export function readPolicyDraft(id?: SavedRevision['id'], slot?: string): PolicyDraft | null {
  return listPolicyDrafts().find((draft) => String(draft.base?.id ?? '') === String(id ?? '') &&
    (slot === undefined || (draft.slot ?? 'legacy') === slot)) ?? null;
}

export function savePolicyDraft(draft: PolicyDraft) {
  localStorage.setItem(draftKey(draft.base?.id, draft.slot), JSON.stringify(draft));
}

export function removePolicyDraft(draft: PolicyDraft) {
  const key = draftKey(draft.base?.id, draft.slot);
  // Preserve a draft saved by another tab since this form was opened.
  if (localStorage.getItem(key) === JSON.stringify(draft)) localStorage.removeItem(key);
}

export function draftPath(draft: PolicyDraft) {
  const path = draft.base ? `/policies/${encodeURIComponent(draft.base.id)}/edit` : '/policies/new';
  return `${path}?${new URLSearchParams({ draft: draft.slot ?? 'legacy' })}`;
}
