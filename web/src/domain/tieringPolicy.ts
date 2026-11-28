export const steps = [
  { id: 'basics', title: '基本信息' },
  { id: 'filter', title: '文件筛选策略' },
  { id: 'rules', title: '冷热分层策略' },
  { id: 'trigger', title: '触发策略' },
  { id: 'execution', title: '执行参数' },
  { id: 'review', title: '确认与预览' },
] as const;
export type WizardStep = typeof steps[number]['id'];
export const poolMigration = 'lustre.pool_migrate';
export const builtinPreview = 'builtin-preview';
export const week = ['mon', 'tue', 'wed', 'thu', 'fri', 'sat', 'sun'];

export const sizeUnits = { B: 'B', KiB: 'KiB', MiB: 'MiB', GiB: 'GiB', TiB: 'TiB', PiB: 'PiB' };
export const intervalUnits = { m: '分钟', h: '小时', d: '天' };
export const durationUnits = { ms: '毫秒', s: '秒', ...intervalUnits };
export const conditionFields = {
  'object.size': '文件大小',
  'object.last_access': '最后访问时间',
  'object.last_modified': '最后修改时间',
};
export const comparisons = { eq: '=', ne: '≠', lt: '<', lte: '≤', gt: '>', gte: '≥' };
const sizeFactors = Object.fromEntries(Object.keys(sizeUnits).map((unit, i) => [unit, BigInt(1024 ** i)]));
const durationFactors = { ms: BigInt(1), s: BigInt(1000), m: BigInt(60000), h: BigInt(3600000), d: BigInt(86400000) };
const maxU64 = BigInt('18446744073709551615');
const maxI64 = BigInt('9223372036854775807');

type Join = 'all' | 'any';
type ActivationMode = 'preview' | 'enforced';
export type Field = keyof typeof conditionFields;
export type FieldPath = (string | number)[];
export interface Quantity { amount: string; unit: string }
export interface Clause extends Quantity { field: Field; op: string }
export interface ConditionGroup { join: Join; clauses: Clause[] }
export interface Rule { name: string; decision: 'execute' | 'skip'; condition: ConditionGroup }

export interface WizardValues {
  name: string;
  description: string;
  filesystem: string;
  sourcePool: string;
  targetPool: string;
  scoped: boolean;
  root: string;
  excludedSubtrees: string[];
  include: ConditionGroup;
  exclude: ConditionGroup;
  rules: Rule[];
  interval: Quantity;
  windowEnabled: boolean;
  timezone: string;
  start: string;
  end: string;
  maxOperations: string;
  maxSubmitAttempts: string;
  maxLogicalBytes: Quantity;
  maxDuration: Quantity;
  maxFileBytes: Quantity;
  profile: string;
  authorization: string;
  approval: string;
}

export interface PolicySettings {
  enabled: boolean;
  mode: ActivationMode;
}

export interface DirectoryScope { root: string; excludedSubtrees: string[] }
type Condition = true | { all: ClauseExpression[] } | { any: ClauseExpression[] };
interface ClauseExpression { field: Field; op: string; value: string | 0 }
export interface PolicyAction {
  type?: string;
  params?: { sourcePool?: string; targetPool?: string; maxFileBytes?: string | number };
}
interface RuleEntry {
  name?: string;
  priority?: number;
  match: unknown;
  decision: string;
  action?: PolicyAction;
}

export interface PolicyDefinition {
  apiVersion: string;
  kind: string;
  metadata: { name: string; description?: string };
  spec: {
    enabled?: boolean;
    target: { filesystem: string; directoryScope?: DirectoryScope; where?: unknown };
    trigger: { type: string; interval?: string };
    rules: { defaults?: { action?: PolicyAction }; entries: RuleEntry[]; exclude?: unknown[] };
    execution: {
      profileRef: string;
      activation?: { mode: ActivationMode };
      admissionWindow?: { timezone: string; weekly: { days: string[]; start: string; end: string }[] };
      budget: { run: {
        maxOperations: number;
        maxSubmitAttempts: number;
        maxObjectLogicalBytes: string | number;
        maxDuration?: string;
      } };
    };
    governance: { authorizationRef: string; approvalPolicyRef: string };
  };
}

export function emptyGroup(): ConditionGroup {
  return { join: 'all', clauses: [] };
}

export function newRule(position: number): Rule {
  return { name: `规则 ${position}`, decision: 'execute', condition: emptyGroup() };
}

export function newWizardValues(): WizardValues {
  return {
    name: '', description: '', filesystem: '', sourcePool: '', targetPool: '',
    scoped: true, root: '/', excludedSubtrees: [], include: emptyGroup(), exclude: emptyGroup(),
    rules: [newRule(1)], interval: { amount: '1', unit: 'h' }, windowEnabled: false,
    timezone: 'Asia/Shanghai', start: '22:00', end: '06:00',
    maxOperations: '100', maxSubmitAttempts: '100',
    maxLogicalBytes: { amount: '100', unit: 'GiB' },
    maxDuration: { amount: '30', unit: 'm' },
    maxFileBytes: { amount: '10', unit: 'GiB' },
    profile: builtinPreview, authorization: builtinPreview, approval: builtinPreview,
  };
}

export function quantityText({ amount, unit }: Quantity) {
  return `${amount}${unit}`;
}

function conditionExpression({ join, clauses }: ConditionGroup): Condition {
  const expressions = clauses.map(({ field, op, amount, unit }): ClauseExpression => ({
    field, op, value: field === 'object.size' && amount === '0' ? 0 : quantityText({ amount, unit }),
  }));
  if (!expressions.length) return true;
  return join === 'all' ? { all: expressions } : { any: expressions };
}

export function policyDocument(values: WizardValues, settings: PolicySettings) {
  const { name, description, filesystem, sourcePool, targetPool, root, excludedSubtrees } = values;
  return {
    apiVersion: 'hsm.storage.io/v1alpha3',
    kind: 'Policy',
    metadata: { name, ...(description && { description }) },
    spec: {
      enabled: settings.enabled,
      target: {
        filesystem,
        objectType: 'file',
        where: conditionExpression(values.include),
        ...(values.scoped && { directoryScope: { root, excludedSubtrees } }),
      },
      trigger: { type: 'periodic', interval: quantityText(values.interval) },
      objective: { type: 'exhaust' },
      rules: {
        defaults: { action: { type: poolMigration, params: { sourcePool, targetPool, maxFileBytes: quantityText(values.maxFileBytes) } } },
        exclude: values.exclude.clauses.length ? [conditionExpression(values.exclude)] : [],
        entries: values.rules.map(({ name: label, decision, condition }, index) => ({
          name: label.trim(), priority: values.rules.length - index, match: conditionExpression(condition), decision,
        })),
      },
      execution: {
        profileRef: values.profile,
        activation: { mode: settings.mode },
        ...(values.windowEnabled && {
          admissionWindow: { timezone: values.timezone.trim(), weekly: [{ days: week, start: values.start, end: values.end }] },
        }),
        budget: {
          run: {
            maxOperations: Number(values.maxOperations), maxSubmitAttempts: Number(values.maxSubmitAttempts),
            maxObjectLogicalBytes: quantityText(values.maxLogicalBytes),
            maxDuration: quantityText(values.maxDuration),
          },
        },
      },
      governance: { authorizationRef: values.authorization, approvalPolicyRef: values.approval },
    },
  };
}

function readQuantity(value: string | number, units: Record<string, string>): Quantity {
  const parsed = typeof value === 'string' && /^(\d+)([a-zA-Z]+)$/.exec(value);
  if (parsed && Object.hasOwn(units, parsed[2])) return { amount: parsed[1], unit: parsed[2] };
  if (value === 0 && Object.hasOwn(units, 'B')) return { amount: '0', unit: 'B' };
  throw new Error('Unsupported quantity');
}

function readGroup(expression: unknown): ConditionGroup {
  if (expression === true) return emptyGroup();
  const group = expression as { all?: ClauseExpression[]; any?: ClauseExpression[] };
  const join = group.all ? 'all' : 'any';
  const clauses = group[join]?.map(({ field, op, value }) => {
    if (!Object.hasOwn(conditionFields, field)) throw new Error('Unsupported policy condition');
    const supported = field === 'object.size' ? Object.hasOwn(comparisons, op) : op === 'older_than';
    if (!supported) throw new Error('Unsupported comparison');
    return { field, op, ...readQuantity(value, field === 'object.size' ? sizeUnits : durationUnits) };
  });
  if (!clauses?.length) throw new Error('Unsupported policy condition group');
  return { join, clauses };
}

function equalJson(left: unknown, right: unknown): boolean {
  if (left === right) return true;
  if (left === null || right === null || typeof left !== 'object' || typeof right !== 'object') return false;
  if (Array.isArray(left) !== Array.isArray(right)) return false;
  const a = Object.entries(left);
  const b = right as Record<string, unknown>;
  return a.length === Object.keys(b).length && a.every(([key, value]) => Object.hasOwn(b, key) && equalJson(value, b[key]));
}

export function readEditablePolicy(document: PolicyDefinition) {
  try {
    const { metadata, spec } = document;
    const values = newWizardValues();
    const params = spec.rules.defaults?.action?.params;
    const scope = spec.target.directoryScope;
    const budget = spec.execution.budget.run;
    const window = spec.execution.admissionWindow;
    if (!params?.sourcePool || !params.targetPool || !params.maxFileBytes || !budget.maxDuration || !spec.trigger.interval) return null;
    if (!Number.isSafeInteger(budget.maxOperations) || !Number.isSafeInteger(budget.maxSubmitAttempts)) return null;
    const settings: PolicySettings = { enabled: spec.enabled !== false, mode: spec.execution.activation?.mode ?? 'preview' };
    const decoded: WizardValues = {
      ...values,
      name: metadata.name, description: metadata.description ?? '', filesystem: spec.target.filesystem,
      sourcePool: params.sourcePool, targetPool: params.targetPool, scoped: !!scope,
      root: scope?.root ?? '/', excludedSubtrees: scope?.excludedSubtrees ?? [],
      include: readGroup(spec.target.where), exclude: readGroup(spec.rules.exclude?.[0] ?? true),
      rules: spec.rules.entries.map(({ name, decision, match }) => {
        if (!name || (decision !== 'skip' && decision !== 'execute')) throw new Error('Unsupported rule');
        return { name, decision, condition: readGroup(match) };
      }),
      interval: readQuantity(spec.trigger.interval, intervalUnits),
      windowEnabled: !!window, timezone: window?.timezone ?? values.timezone,
      start: window?.weekly[0]?.start ?? values.start, end: window?.weekly[0]?.end ?? values.end,
      maxOperations: String(budget.maxOperations), maxSubmitAttempts: String(budget.maxSubmitAttempts),
      maxLogicalBytes: readQuantity(budget.maxObjectLogicalBytes, sizeUnits),
      maxDuration: readQuantity(budget.maxDuration, durationUnits),
      maxFileBytes: readQuantity(params.maxFileBytes, sizeUnits),
      profile: spec.execution.profileRef,
      authorization: spec.governance.authorizationRef, approval: spec.governance.approvalPolicyRef,
    };
    // Only configurations that can be submitted without losing fields are editable.
    return equalJson(document, policyDocument(decoded, settings)) ? { values: decoded, settings } : null;
  } catch {
    return null;
  }
}

export function policyPoolPair(definition: PolicyDefinition) {
  const { defaults, entries } = definition.spec.rules;
  const base = defaults?.action;
  const rule = entries.find(({ decision, action }) => decision !== 'skip' && (action?.type ?? base?.type) === poolMigration);
  const dormant = definition.spec.enabled === false && definition.spec.target.directoryScope &&
    base?.type === poolMigration && entries.every(({ decision }) => decision === 'skip');
  if (!rule && !dormant) return null;
  const { sourcePool, targetPool } = {
    ...(base?.type === poolMigration ? base.params : undefined),
    ...rule?.action?.params,
  };
  return sourcePool && targetPool ? { sourcePool, targetPool } : null;
}

type ResourcesForValidation = {
  filesystems: { id: string; tieringAvailable: boolean; pools: { name: string; eligible: boolean }[] | null }[];
  profile: { name: string }[];
  authorization: { name: string }[];
  approval: { name: string }[];
};

export interface FieldIssue { name: FieldPath; message: string }

function integer(value: string, zero = false) {
  if (typeof value !== 'string' || value.length > 20 || !/^(0|[1-9]\d*)$/.test(value)) return undefined;
  const result = BigInt(value);
  return result > 0 || zero ? result : undefined;
}

function quantity(value: Quantity | undefined, units: Record<string, bigint>, zero = false) {
  if (!value) return undefined;
  const amount = integer(value.amount, zero);
  const factor = units[value.unit];
  return amount === undefined || typeof factor !== 'bigint' ? undefined : amount * factor;
}

function pathComponents(path: string | undefined) {
  if (!path?.startsWith('/') || path.includes('\0')) return undefined;
  const bytes = new TextEncoder();
  if (bytes.encode(path).length > 4095) return undefined;
  const parts = path.split('/').filter(Boolean);
  return parts.some((part) => part === '.' || part === '..' || bytes.encode(part).length > 255)
    ? undefined : parts;
}

function validTimezone(zone: string) {
  try {
    if (!zone.trim()) return false;
    new Intl.DateTimeFormat('en', { timeZone: zone.trim() });
    return true;
  } catch { return false; }
}

export function validateStep(step: WizardStep, v: WizardValues, resources: ResourcesForValidation, settings: PolicySettings) {
  const issues: FieldIssue[] = [];
  const report = (name: FieldPath, message: string) => issues.push({ name, message });
  const checkQuantity = (field: FieldPath, input: Quantity | undefined, factors: Record<string, bigint>, limit: bigint, zero = false) => {
    const size = quantity(input, factors, zero);
    if (size === undefined || size > limit) report([...field, 'amount'], '请输入支持范围内的整数和单位');
  };
  const checkGroup = (field: FieldPath, group: ConditionGroup) => {
    group.clauses.forEach((clause, i) => {
      const path = [...field, 'clauses', i];
      if (clause.field === 'object.size') {
        checkQuantity(path, clause, sizeFactors, maxI64, true);
      } else {
        checkQuantity(path, clause, durationFactors, BigInt(90_000_000) * durationFactors.d);
      }
    });
  };
  switch (step) {
    case 'basics': {
      if (!v.name) report(['name'], '请输入策略名称');
      else if (!/^[a-z0-9](?:[a-z0-9.-]{0,251}[a-z0-9])?$/.test(v.name)) {
        report(['name'], '名称须为 1–253 个小写字母、数字、点或连字符，首尾须为字母或数字');
      }
      const fs = resources.filesystems.find(({ id, tieringAvailable }) => tieringAvailable && id === v.filesystem);
      if (!fs) report(['filesystem'], '请选择可用的 Lustre 文件系统');
      const pools = fs?.pools?.filter(({ eligible }) => eligible) ?? [];
      if (!pools.some(({ name }) => name === v.sourcePool)) report(['sourcePool'], '请选择有效源池');
      if (v.sourcePool === v.targetPool || !pools.some(({ name }) => name === v.targetPool)) {
        report(['targetPool'], '请选择与源池不同的有效目标池');
      }
      break;
    }
    case 'filter': {
      if (v.scoped) {
        const root = pathComponents(v.root);
        if (!root) report(['root'], '请输入有效的文件系统根下绝对路径');
        v.excludedSubtrees.forEach((path, index) => {
          const parts = pathComponents(path);
          if (!root || !parts || parts.length <= root.length || root.some((part, i) => parts[i] !== part)) {
            report(['excludedSubtrees', index], '排除目录须为起始目录的子目录');
          }
        });
      }
      checkGroup(['include'], v.include);
      checkGroup(['exclude'], v.exclude);
      break;
    }
    case 'rules': {
      const names = new Set<string>();
      v.rules.forEach(({ name, condition }, i) => {
        const label = name?.trim();
        if (!label || names.has(label)) report(['rules', i, 'name'], '规则名称须非空且唯一');
        names.add(label);
        checkGroup(['rules', i, 'condition'], condition);
      });
      if (!v.rules.length || ((settings.enabled || !v.scoped) && !v.rules.some(({ decision }) => decision === 'execute'))) {
        report(['rules'], '当前配置至少需要一条池迁移规则');
      }
      break;
    }
    case 'trigger':
      checkQuantity(['interval'], v.interval,
        { m: durationFactors.m, h: durationFactors.h, d: durationFactors.d },
        BigInt(Date.UTC(10000, 0, 1) - Date.now() - 1));
      if (v.windowEnabled) {
        if (!validTimezone(v.timezone)) report(['timezone'], '请输入有效的 IANA 时区');
        const time = /^(?:[01]\d|2[0-3]):[0-5]\d$/;
        if (!time.test(v.start)) report(['start'], '请输入开始时刻');
        if (!time.test(v.end) || v.end === v.start) report(['end'], '结束时刻须有效且不同于开始时刻');
      }
      break;
    case 'execution':
      for (const key of ['maxOperations', 'maxSubmitAttempts'] as const) {
        const value = integer(v[key]);
        if (value === undefined || value > BigInt(Number.MAX_SAFE_INTEGER)) report([key], '请输入支持范围内的正整数');
      }
      checkQuantity(['maxLogicalBytes'], v.maxLogicalBytes, sizeFactors, maxU64);
      checkQuantity(['maxDuration'], v.maxDuration, durationFactors, maxU64);
      checkQuantity(['maxFileBytes'], v.maxFileBytes, sizeFactors, maxI64);
      for (const key of ['profile', 'authorization', 'approval'] as const) {
        if (!resources[key].some(({ name }) => name === v[key])) report([key], '请选择当前可用的资源');
      }
      break;
  }
  return issues;
}
