import { execFileSync } from 'node:child_process';
import { linkSync, mkdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { readFile } from 'node:fs/promises';
import { join, relative } from 'node:path';
import { expect, test, type APIRequestContext } from '@playwright/test';
import type { FileOperation } from '../src/services/executions';

const action = 'lustre.pool_migrate';

test('an operator creates a scoped policy in the console and downloads real Lustre results', { tag: '@daemon' }, async ({ page, request }) => {
  const fixture = {
    RBH_CONSOLE_EXECUTION_TEST_ROOT: process.env.RBH_CONSOLE_EXECUTION_TEST_ROOT,
    RBH_CONSOLE_EXECUTION_TEST_MOUNT: process.env.RBH_CONSOLE_EXECUTION_TEST_MOUNT,
    RBH_CONSOLE_EXECUTION_TEST_FS: process.env.RBH_CONSOLE_EXECUTION_TEST_FS,
    RBH_CONSOLE_EXECUTION_TEST_TOKEN: process.env.RBH_CONSOLE_EXECUTION_TEST_TOKEN,
  };
  test.skip(Object.values(fixture).every((value) => value === undefined), 'requires a disposable Lustre directory, daemon and writer');
  const missing = Object.entries(fixture).filter(([, value]) => !value?.trim()).map(([name]) => name);
  if (missing.length > 0) throw new Error(`Missing Lustre console test settings: ${missing.join(', ')}`);
  const {
    RBH_CONSOLE_EXECUTION_TEST_ROOT: root, RBH_CONSOLE_EXECUTION_TEST_MOUNT: mount,
    RBH_CONSOLE_EXECUTION_TEST_FS: filesystem, RBH_CONSOLE_EXECUTION_TEST_TOKEN: token,
  } = fixture;
  test.setTimeout(120_000);
  const writer = { Authorization: `Bearer ${token}` };
  const lfs = (...args: string[]) => execFileSync('lfs', args, { encoding: 'utf8' }).trim();
  const fid = (file: string) => lfs('path2fid', file);
  const pool = (file: string) => lfs('getstripe', '-p', file);
  const ost = (file: string) => Number(lfs('getstripe', '-i', file));
  const files = ['team/allowed', 'team/outside', 'team/excluded', 'team/archived', 'team/periodic', 'team-other/unscoped']
    .map((name) => join(root!, name));
  const allowed = files[0];
  const alias = join(root!, 'team', 'allowed-link');
  mkdirSync(join(root!, 'team', 'archive'), { recursive: true });
  mkdirSync(join(root!, 'team-other'));
  files.forEach((file, index) => {
    lfs('setstripe', '-p', 'hot', '-c', '1', file);
    writeFileSync(file, Buffer.alloc((index + 1) * 100, index + 1));
  });
  linkSync(allowed, alias);
  linkSync(files[1], join(root!, 'team-other', 'outside-link'));
  linkSync(files[2], join(root!, 'team', 'archive', 'excluded-link'));
  const periodicAlias = join(root!, 'team', 'periodic-link');
  linkSync(files[4], periodicAlias);
  lfs('hsm_set', '--exists', '--archived', '--archive-id', '17', files[3]);

  const ids = files.map(fid);
  const originalOsts = files.map(ost);
  const label = `execution-${Date.now()}`;
  for (const [path, kind, spec] of [
    ['authorization-policies', 'AuthorizationPolicy', {
      defaultDecision: 'deny',
      statements: [{ id: 'tiering', actions: [action], decision: 'allow',
        subjects: { serviceAccounts: ['tiering-daemon'] },
        scope: { filesystems: [filesystem], objectTypes: ['file'] } }],
    }],
    ['approval-policies', 'ApprovalPolicy', {
      defaultDecision: 'deny', requirements: [{ id: 'tiering', actions: [action], mode: 'not_required' }],
      floorDefaults: { minApprovers: 1, approverRoles: ['approver'] },
    }],
    ['system-safety-policies', 'SystemSafetyPolicy', { scope: { filesystems: [filesystem] }, allowedActions: [action] }],
    ['execution-profiles', 'ExecutionProfile', {
      concurrency: { global: 2 },
      retry: { maxAttempts: 1, backoff: 'fixed', initialDelay: '1s', maxDelay: '1s', retryableErrors: ['transient_io'] },
      workerLease: { ttl: '1m', renewEvery: '10s', onLeaseLost: 'stop_and_reconcile' },
      circuitBreaker: { scopes: [{
        domain: { key: 'policy' }, errorClasses: ['backend_unavailable'], evaluation: 'any',
        conditions: [{ type: 'consecutive_failures', threshold: 1 }],
        onOpen: 'suspend_submissions', resetMode: 'manual',
      }] },
    }],
  ] as const) {
    const response = await request.post(`/api/${path}`, {
      headers: writer, data: { apiVersion: 'hsm.storage.io/v1alpha3', kind, metadata: { name: label }, spec },
    });
    expect(response.status(), await response.text()).toBe(201);
  }

  const scanFixture = async () => {
    const scan = await request.post('/api/scans', { data: { filesystem, root } });
    expect(scan.status()).toBe(202);
    const scanId = (await scan.json()).id;
    await expect.poll(async () => (await (await request.get(`/api/scans/${scanId}`)).json()).state).toBe('completed');
    expect((await (await request.get(`/api/scans/${scanId}`)).json()).errors).toBe(0);
  };
  await scanFixture();

  const team = '/' + relative(mount!, join(root!, 'team'));
  const document = {
    apiVersion: 'hsm.storage.io/v1alpha3', kind: 'Policy', metadata: { name: label },
    spec: {
      enabled: false,
      target: {
        filesystem, objectType: 'file',
        where: { all: [{ field: 'object.size', op: 'lte', value: '400B' }] },
        directoryScope: { root: team, excludedSubtrees: [`${team}/archive`] },
      },
      trigger: { type: 'periodic', interval: '1h' }, objective: { type: 'exhaust' },
      rules: {
        defaults: { action: { type: action, params: { sourcePool: 'hot', targetPool: 'cold', maxFileBytes: '1MiB' } } },
        exclude: [], entries: [{ name: '迁移', priority: 1, match: true, decision: 'execute' }],
      },
      execution: {
        profileRef: 'builtin-preview', activation: { mode: 'enforced' },
        budget: { run: { maxOperations: 10, maxSubmitAttempts: 10, maxObjectLogicalBytes: '1MiB', maxDuration: '1m' } },
      },
      governance: { authorizationRef: label, approvalPolicyRef: label },
    },
  };
  const create = async (data: object) => {
    const response = await request.post('/api/policies', { headers: writer, data });
    expect(response.status(), await response.text()).toBe(201);
    return (await response.json()).idExact as string;
  };
  async function confirm(actionName: string) {
    const dialog = page.getByRole('dialog', { name: actionName });
    const submit = dialog.getByRole('button', { name: actionName });
    await submit.click();
    await dialog.getByLabel('策略写入令牌').fill(token!);
    await submit.click();
  }

  await page.goto('/console/#/policies/new');
  const select = async (label: string, option: string | RegExp) => {
    const control = page.getByLabel(label, { exact: true }).and(page.getByRole('combobox'));
    await control.press('ArrowDown');
    const list = page.locator(`[id="${await control.getAttribute('aria-controls')}"]`);
    await list.getByRole('option', { name: option, exact: true }).click();
  };
  const next = () => page.getByRole('button', { name: '下一步' }).click();
  await page.getByLabel('策略名称').fill(label);
  await select('Lustre 文件系统', filesystem!);
  await select('源池', /^hot (?:可用|容量)/);
  await select('目标池', /^cold (?:可用|容量)/);
  await next();
  await page.getByLabel('起始目录').fill(team);
  await page.getByRole('button', { name: '添加排除目录' }).click();
  await page.getByLabel('排除子目录 1').fill(`${team}/archive`);
  await page.getByRole('button', { name: '添加条件' }).first().click();
  await select('比较', '≤');
  await select('单位', 'B');
  await page.getByLabel('数值').fill('300');
  await next();
  await page.getByLabel('规则名称 1').fill('迁移');
  await next();
  await select('单位', '分钟');
  await next();
  const governanceOption = new RegExp(`^${label} · 修订 `);
  await select('授权策略', governanceOption);
  await select('审批策略', governanceOption);
  await select('执行模式', '实际执行');
  await next();
  await page.getByRole('button', { name: '计算预览' }).click();
  const preview = page.getByRole('region', { name: '预览结果' });
  await expect(preview.locator('.ant-statistic-content').first()).toHaveText('1');
  for (let step = 0; step < 4; step += 1) {
    await page.getByRole('button', { name: '上一步' }).click();
  }
  await page.getByLabel('数值').fill('400');
  for (let step = 0; step < 4; step += 1) await next();
  await expect(preview).toHaveCount(0);
  await page.getByRole('button', { name: '计算预览' }).click();
  await expect(preview).toContainText('链接位于范围外');
  await expect(preview).toContainText('链接位于排除子树');
  await expect(preview.locator('.ant-statistic-content').first()).toHaveText('2');
  expect((await (await request.get('/api/runs')).json()).items).toEqual([]);
  await page.getByRole('button', { name: '保存策略' }).click();
  const save = page.getByRole('dialog', { name: '策略写入凭据' });
  await save.getByLabel('策略写入令牌').fill(token!);
  await save.getByRole('button', { name: '提交保存' }).click();
  await expect(page).toHaveURL(/#\/policies\/\d+$/);
  const policy = page.url().split('/').at(-1)!;
  const saved = await (await request.get(`/api/policies/${policy}`)).json();
  expect(saved.definition.spec).toMatchObject({ enabled: false, trigger: { type: 'periodic', interval: '1m' } });
  await expect(page.getByRole('button', { name: '立即运行' })).toBeDisabled();
  await page.getByRole('button', { name: '启用策略' }).click();
  await confirm('启用策略');
  await expect(page.getByText('修订 2', { exact: true })).toBeVisible();

  await page.getByRole('link', { name: '返回策略列表' }).click();
  const policyRow = page.getByRole('row').filter({ hasText: label });
  await policyRow.getByRole('button', { name: '立即运行' }).click();
  await confirm('立即运行');
  await expect(page).toHaveURL(/#\/runs\/\d+$/);
  const manualRun = page.url().split('/').at(-1)!;
  await expect.poll(async () => (await readRun(request, manualRun)).state).toBe('finished');
  await page.getByRole('tab', { name: '逐文件明细' }).click();
  await page.getByRole('button', { name: '刷新', exact: true }).click();
  await expect(page.getByRole('row').filter({ hasText: ids[0] })).toContainText('已完成');
  await expect(page.getByRole('row').filter({ hasText: ids[1] })).toContainText('link_outside_scope');

  const run = await readRun(request, manualRun);
  expect(run).toMatchObject({ revisionExact: '2', policyIdExact: policy, policyName: label });
  expect(run.requestedBy).not.toBeNull();
  expect(run.directoryScope).toMatchObject({
    excludedByReason: { link_outside_scope: 1, link_in_excluded_subtree: 1 },
  });
  await page.getByRole('tab', { name: '报告', exact: true }).click();
  const downloadEvent = page.waitForEvent('download');
  await page.getByRole('link', { name: '导出运行报告' }).click();
  const download = await downloadEvent;
  expect(download.suggestedFilename()).toBe(`run-${manualRun}.json`);
  const report = JSON.parse(await readFile(await download.path(), 'utf8'));
  expect(report.run.directoryScope).toEqual(run.directoryScope);
  expect(report.operationResults).toMatchObject({ matchedCount: 4, completedLogicalBytes: '100' });
  expect(report.policyRevision.definition.metadata.name).toBe(label);
  const operations = await (await request.get(`/api/runs/${manualRun}/operations`)).json();
  expect(operations.items).toHaveLength(4);
  const byFid = Object.fromEntries(operations.items.map((operation: { fid: string }) => [operation.fid, operation]));
  expect(byFid[ids[0]]).toMatchObject({
    state: 'completed', completionEvidence: { pool: 'cold', ostIndices: [ost(allowed)] },
  });
  expect(byFid[ids[1]]).toMatchObject({ state: 'deferred', reason: 'link_outside_scope', attempted: false });
  expect(byFid[ids[2]]).toMatchObject({ state: 'deferred', reason: 'link_in_excluded_subtree', attempted: false });
  expect(byFid[ids[3]]).toMatchObject({ state: 'deferred', reason: 'hsm_state_ineligible', attempted: false });

  const fsName = lfs('getname', '-n', mount!);
  const members = lfs('pool_list', `${fsName}.cold`).split('\n').flatMap((line) => {
    const index = /-OST([0-9a-f]+)_UUID/i.exec(line);
    return index ? [Number.parseInt(index[1], 16)] : [];
  });
  expect(members).toContain(ost(allowed));
  expect(pool(allowed)).toBe('cold');
  expect(fid(alias)).toBe(ids[0]);
  expect(statSync(alias, { bigint: true }).nlink).toBe(2n);
  expect(readFileSync(alias)).toEqual(Buffer.alloc(100, 1));
  for (const index of [1, 2, 3]) {
    expect(fid(files[index])).toBe(ids[index]);
    expect(pool(files[index])).toBe('hot');
    expect(ost(files[index])).toBe(originalOsts[index]);
  }

  // Make a second file eligible after the manual run so the periodic trigger
  // can migrate it using the same policy revision.
  writeFileSync(files[4], Buffer.alloc(125, 5));
  await scanFixture();
  const periodicRun = async () => {
    const response = await request.get(`/api/runs?policyId=${policy}&state=finished`);
    expect(response.ok()).toBe(true);
    return (await response.json()).items.find(({ requestedBy }: { requestedBy: string | null }) => requestedBy === null);
  };
  await expect.poll(periodicRun, { timeout: 75_000, intervals: [1_000] }).toBeTruthy();
  const periodic = await periodicRun();
  expect(periodic).toMatchObject({ policyIdExact: policy, revisionExact: '2', requestedBy: null });
  const periodicOperations = await request.get(`/api/runs/${periodic.idExact}/operations?fid=${encodeURIComponent(ids[4])}`);
  expect((await periodicOperations.json()).items).toMatchObject([{
    fid: ids[4], state: 'completed', completionEvidence: { pool: 'cold', ostIndices: [ost(files[4])] },
  }]);
  expect(pool(files[4])).toBe('cold');
  expect(members).toContain(ost(files[4]));
  expect(fid(files[4])).toBe(ids[4]);
  expect(fid(periodicAlias)).toBe(ids[4]);
  expect(statSync(periodicAlias, { bigint: true }).nlink).toBe(2n);
  expect(readFileSync(periodicAlias)).toEqual(Buffer.alloc(125, 5));

  await page.goto(`/console/#/policies/${policy}`);
  await page.getByRole('button', { name: '停用策略' }).click();
  await confirm('停用策略');
  await expect(page.getByText('修订 3', { exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: '立即运行' })).toBeDisabled();
  expect((await readRun(request, manualRun)).revisionExact).toBe('2');

  const unscoped = structuredClone(document);
  unscoped.metadata.name += '-unscoped';
  unscoped.spec.enabled = true;
  unscoped.spec.target.where.all[0] = { field: 'object.size', op: 'eq', value: '600B' };
  const { directoryScope, ...target } = unscoped.spec.target;
  const unscopedId = await create({ ...unscoped, spec: { ...unscoped.spec, target } });
  const submitted = await request.post(`/api/policies/${unscopedId}/run`, { headers: writer });
  expect(submitted.status(), await submitted.text()).toBe(202);
  const unscopedRun = (await submitted.json()).runIdExact;
  await expect.poll(async () => (await readRun(request, unscopedRun)).state).toBe('finished');
  expect(pool(files[5])).toBe('cold');
  expect(members).toContain(ost(files[5]));
  expect(fid(files[5])).toBe(ids[5]);

  // Limit copy bandwidth so both operations remain observable through the run API.
  const parallelFiles = ['parallel-a', 'parallel-b'].map((name) => join(root!, 'team', name));
  const parallelContent = parallelFiles.map((_, index) => Buffer.alloc(4 * 1024 * 1024, index + 1));
  parallelFiles.forEach((file, index) => {
    lfs('setstripe', '-p', 'hot', '-c', '1', file);
    writeFileSync(file, parallelContent[index]);
  });
  const parallelFids = parallelFiles.map(fid);
  await scanFixture();
  const parallel = structuredClone(document);
  parallel.metadata.name += '-parallel';
  parallel.spec.enabled = true;
  parallel.spec.target.where.all[0] = { field: 'object.size', op: 'eq', value: '4MiB' };
  parallel.spec.execution.profileRef = label;
  parallel.spec.execution.budget.run.maxObjectLogicalBytes = '8MiB';
  const params = {
    ...parallel.spec.rules.defaults.action.params, maxFileBytes: '4MiB', maxConcurrency: 2, maxMibPerSecond: 2,
  };
  const parallelId = await create({
    ...parallel,
    spec: {
      ...parallel.spec,
      rules: { ...parallel.spec.rules, defaults: { action: { type: action, params } } },
    },
  });
  const parallelSubmission = await request.post(`/api/policies/${parallelId}/run`, { headers: writer });
  expect(parallelSubmission.status(), await parallelSubmission.text()).toBe(202);
  const parallelRun = (await parallelSubmission.json()).runIdExact;
  const parallelOperations = async (): Promise<FileOperation[]> => {
    const response = await request.get(`/api/runs/${parallelRun}/operations`);
    expect(response.ok()).toBe(true);
    return (await response.json()).items;
  };
  await expect.poll(async () => (await parallelOperations())
    .filter(({ state }) => state === 'submitting').map(({ fid }) => fid).sort(),
  { intervals: [100], timeout: 10_000 }).toEqual([...parallelFids].sort());
  await expect.poll(async () => (await readRun(request, parallelRun)).state, { timeout: 20_000 }).toBe('finished');
  expect(await parallelOperations()).toEqual(expect.arrayContaining(parallelFids.map((fid) => expect.objectContaining({
    fid, state: 'completed', completionEvidence: expect.objectContaining({ pool: 'cold' }),
  }))));
  parallelFiles.forEach((file, index) => {
    expect(fid(file)).toBe(parallelFids[index]);
    expect(pool(file)).toBe('cold');
    expect(readFileSync(file).equals(parallelContent[index])).toBe(true);
  });
});

async function readRun(request: APIRequestContext, id: string) {
  const response = await request.get(`/api/runs/${id}`);
  expect(response.ok()).toBe(true);
  return response.json();
}
