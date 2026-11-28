import { execFileSync } from 'node:child_process';
import { linkSync, mkdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { join, relative } from 'node:path';
import { expect, test, type APIRequestContext } from '@playwright/test';

const action = 'lustre.pool_migrate';

test('the daemon and console run an enabled policy and preserve excluded Lustre layouts', async ({ page, request }) => {
  const root = process.env.RBH_CONSOLE_EXECUTION_TEST_ROOT;
  const mount = process.env.RBH_CONSOLE_EXECUTION_TEST_MOUNT;
  const filesystem = process.env.RBH_CONSOLE_EXECUTION_TEST_FS;
  const token = process.env.RBH_CONSOLE_EXECUTION_TEST_TOKEN;
  test.skip(!root || !mount || !filesystem || !token, 'requires a disposable Lustre directory, daemon and writer');
  test.setTimeout(90_000);
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
  ] as const) {
    const response = await request.post(`/api/${path}`, {
      headers: writer, data: { apiVersion: 'hsm.storage.io/v1alpha3', kind, metadata: { name: label }, spec },
    });
    expect(response.status(), await response.text()).toBe(201);
  }

  const scan = await request.post('/api/scans', { data: { filesystem, root } });
  expect(scan.status()).toBe(202);
  const scanId = (await scan.json()).id;
  await expect.poll(async () => (await (await request.get(`/api/scans/${scanId}`)).json()).state).toBe('completed');
  expect((await (await request.get(`/api/scans/${scanId}`)).json()).errors).toBe(0);

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
  const update = async (id: string, revision: string, data: object) => {
    const response = await request.put(`/api/policies/${id}`, {
      headers: { ...writer, 'If-Match': `"${revision}"` }, data,
    });
    expect(response.status(), await response.text()).toBe(200);
  };
  async function confirm(actionName: string) {
    const dialog = page.getByRole('dialog', { name: actionName });
    const submit = dialog.getByRole('button', { name: actionName });
    await submit.click();
    await dialog.getByLabel('Rev.8 writer token').fill(token!);
    await submit.click();
  }

  const policy = await create(document);
  await page.goto(`/console/#/policies/${policy}`);
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
  const reportResponse = await request.get(`/api/runs/${manualRun}/reports/run`);
  expect(reportResponse.ok()).toBe(true);
  const report = await reportResponse.json();
  expect(report.run.directoryScope).toEqual(run.directoryScope);
  expect(report.operationResults).toMatchObject({ matchedCount: 4, completedLogicalBytes: '100' });
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

  await page.goto(`/console/#/policies/${policy}`);
  await page.getByRole('button', { name: '停用策略' }).click();
  await confirm('停用策略');
  await expect(page.getByText('修订 3', { exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: '立即运行' })).toBeDisabled();
  expect((await readRun(request, manualRun)).revisionExact).toBe('2');

  const periodic = structuredClone(document);
  periodic.metadata.name += '-periodic';
  periodic.spec.trigger.interval = '1s';
  periodic.spec.target.where.all[0] = { field: 'object.size', op: 'eq', value: '500B' };
  const periodicId = await create(periodic);
  periodic.spec.enabled = true;
  await update(periodicId, '1', periodic);
  const finished = async () => (await (await request.get(`/api/runs?policyId=${periodicId}&state=finished`)).json()).items;
  try {
    await expect.poll(async () => (await finished()).length).toBeGreaterThan(0);
    // The first cycle still contains the operation if later cycles finished
    // after the source layout had already changed.
    const first = (await finished()).at(-1);
    expect(first).toMatchObject({ revisionExact: '2', requestedBy: null });
    const response = await request.get(`/api/runs/${first.idExact}/operations`);
    expect((await response.json()).items).toMatchObject([{
      fid: ids[4], state: 'completed', completionEvidence: { pool: 'cold', ostIndices: [ost(files[4])] },
    }]);
  } finally {
    periodic.spec.enabled = false;
    await update(periodicId, '2', periodic);
  }
  expect(pool(files[4])).toBe('cold');
  expect(members).toContain(ost(files[4]));
  expect(fid(files[4])).toBe(ids[4]);
  expect(fid(periodicAlias)).toBe(ids[4]);
  expect(statSync(periodicAlias, { bigint: true }).nlink).toBe(2n);
  expect(readFileSync(periodicAlias)).toEqual(Buffer.alloc(500, 5));

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
});

async function readRun(request: APIRequestContext, id: string) {
  const response = await request.get(`/api/runs/${id}`);
  expect(response.ok()).toBe(true);
  return response.json();
}
