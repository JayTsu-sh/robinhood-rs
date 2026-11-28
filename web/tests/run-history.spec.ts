import { readFile } from 'node:fs/promises';
import { expect, test } from '@playwright/test';

const run = {
  idExact: '9007199254740993', policyIdExact: '42', revisionExact: '3', policyName: '执行时的策略',
  state: 'finished', reason: null, candidateCount: 105,
  estimatedBytesExact: '9007199254740993', requestedBy: 'operator',
  createdAt: '2026-11-28T06:15:00Z', startedAt: null, activationMode: 'enforced',
  policyMode: 'continuous',
};
const file = {
  idExact: '9007199254740995', filesystem: 'lab-a', fid: '[0x200000401:0x67:0x0]',
  action: 'lustre.pool_migrate', attempted: true, sizeExact: '9007199254740993',
  migrationPlan: { sourcePool: 'hot', targetPool: 'cold', stripeCount: 2, stripeSize: 1048576 },
  acceptedAt: null, finishedAt: null, lastCheckedAt: null,
};
const evidence = { source: 'lustre_layout', pool: 'cold', ostIndices: [8, 9], observedAtMicros: 1795852800000000 };

test('execution history opens a pinned run and refreshes operations after dispatch ends', async ({ page }) => {
  let reconciled = false;
  let failMetadata = false;
  let runQueries = 0;
  const results = () => ({
    matchedCount: 2,
    pendingCount: reconciled ? 0 : 1,
    completedLogicalBytes: reconciled ? file.sizeExact : '0',
    byState: { failed: 1, [reconciled ? 'completed' : 'unknown']: 1 },
  });
  await page.route(/\/api\/runs(?:\?|$)/, (route) => {
    const filters = new URL(route.request().url()).searchParams;
    if (filters.has('beforeId')) {
      expect(filters.get('beforeId')).toBe(run.idExact);
      expect(filters.get('policyName')).toBe('执行时');
    }
    return route.fulfill({ json: {
      items: [filters.has('beforeId') ? { ...run, idExact: '401' } : run],
      nextCursor: filters.has('beforeId') || filters.has('runId') ? null : run.idExact,
    } });
  });
  await page.route(`**/api/runs/${run.idExact}`, (route) => {
    runQueries++;
    if (failMetadata) return route.fulfill({ status: 503, json: { error: '暂时无法读取运行' } });
    return route.fulfill({ json: { ...run, hasPendingOperations: !reconciled, operationResults: results() } });
  });
  await page.route((url) => url.pathname === `/api/runs/${run.idExact}/operations/summary`, (route) =>
    route.fulfill({ json: { ...results(), byReason: [] } }),
  );
  await page.route((url) => url.pathname === `/api/runs/${run.idExact}/operations`, (route) => {
    const filters = new URL(route.request().url()).searchParams;
    if (filters.has('fid')) {
      expect(filters.get('fid')).toBe(file.fid);
      expect(filters.get('reason')).toBeNull();
    }
    const after = filters.get('afterId');
    if (after) expect(after).toBe('9007199254740994');
    const first = !after && !filters.has('fid');
    return route.fulfill({ json: {
      items: [first ? { ...file, idExact: '9007199254740994', fid: '[0x200000401:0x66:0x0]',
        state: 'failed', reason: 'layout_swap_failed', completionEvidence: null } : { ...file, state: reconciled ? 'completed' : 'unknown',
        reason: reconciled ? null : 'migration_reconcile_pending',
        completionEvidence: reconciled ? evidence : null }],
      nextCursor: first ? '9007199254740994' : null,
    } });
  });
  await page.goto('/console/#/policies');
  await page.getByRole('link', { name: '执行记录', exact: true }).click();
  await page.getByLabel('执行时的策略名称').fill('执行时');
  await page.getByRole('button', { name: '查询', exact: true }).click();
  await page.getByRole('button', { name: '下一页' }).click();
  await expect(page.getByRole('link', { name: '#401', exact: true })).toBeVisible();
  await page.getByRole('button', { name: '上一页' }).click();
  await page.getByRole('link', { name: `#${run.idExact}`, exact: true }).click();
  await expect(page.getByRole('heading', { name: `运行 #${run.idExact}` })).toBeVisible();
  await expect(page.getByText('派发已结束', { exact: true })).toBeVisible();
  await expect(page.getByText('仍有文件操作待核对')).toBeVisible();
  await expect(page.getByRole('link', { name: '执行时的策略 · 修订 3' }))
    .toHaveAttribute('href', '#/policies/42/revisions/3');
  await expect(page.getByText('9,007,199,254,740,993 B')).toBeVisible();
  await page.getByRole('tab', { name: '逐文件明细' }).click();
  await page.getByLabel('文件 FID').fill(file.fid);
  await page.getByRole('button', { name: '查询', exact: true }).click();
  await expect(page.getByRole('row').filter({ hasText: file.fid })).toContainText('结果未知');
  await expect(page.getByText('migration_reconcile_pending', { exact: true })).toBeVisible();

  await page.reload();
  await expect(page.getByLabel('文件 FID')).toHaveValue(file.fid);
  await page.getByRole('button', { name: '重置筛选' }).click();
  await page.getByRole('button', { name: '下一页' }).click();
  await expect(page.getByRole('row').filter({ hasText: file.fid })).toContainText('结果未知');
  const initialReads = runQueries;
  reconciled = true;
  failMetadata = true;
  await page.getByRole('button', { name: '刷新', exact: true }).click();
  await expect(page.getByText('刷新运行状态失败')).toBeVisible();
  await expect(page.getByRole('row').filter({ hasText: file.fid })).toContainText('已完成');
  await expect(page.getByText('第 2 页')).toBeVisible();
  failMetadata = false;
  await page.getByRole('button', { name: '刷新', exact: true }).click();
  await expect(page.getByRole('row').filter({ hasText: file.fid })).toContainText('已完成');
  await expect(page.getByText('第 2 页')).toBeVisible();
  expect(runQueries).toBeGreaterThan(initialReads);
  await expect(page.getByText('仍有文件操作待核对')).toHaveCount(0);
  await page.getByRole('button', { name: '查看证据' }).click();
  const details = page.getByRole('dialog', { name: '逐文件证据' });
  await expect(details).toContainText('计划目标池');
  await expect(details).toContainText('实际目标池');
  await expect(details).toContainText('8、9');
});

test('the built console reads a recorded run and file evidence from the daemon', { tag: '@daemon' }, async ({ page, request }) => {
  test.skip(process.env.RBH_CONSOLE_RUN_HISTORY_TEST !== '1', 'requires the isolated MariaDB run history fixture');
  const response = await request.get('/api/runs?policyName=history-files-&limit=1');
  expect(response.ok()).toBe(true);
  const { items: [record] } = await response.json() as {
    items: { idExact: string; policyIdExact: string; policyName: string; revisionExact: string }[];
  };
  expect(record).toBeDefined();

  await page.goto(`/console/#/runs?policyId=${record.policyIdExact}`);
  await page.getByRole('link', { name: `#${record.idExact}`, exact: true }).click();
  const revisionLink = page.getByRole('link', { name: `${record.policyName} · 修订 ${record.revisionExact}` });
  await expect(revisionLink).toBeVisible();
  await page.getByRole('tab', { name: '逐文件明细' }).click();
  await page.getByRole('button', { name: '下一页' }).click();
  await expect(page.getByText('第 2 页')).toBeVisible();
  await page.getByLabel('文件 FID').fill('0x200000401:0x66:0x0');
  await page.getByLabel('原因包含').fill('layout_swap');
  await page.getByRole('button', { name: '查询', exact: true }).click();
  await expect(page.getByRole('row').filter({ hasText: '[0x200000401:0x66:0x0]' })).toContainText('失败');
  await expect(page.getByText('第 1 页')).toBeVisible();
  await page.reload();
  await expect(page.getByLabel('原因包含')).toHaveValue('layout_swap');
  await page.getByRole('button', { name: '重置筛选' }).click();
  await page.getByRole('row').filter({ hasText: '[0x200000401:0x1:0x0]' })
    .getByRole('button', { name: '查看证据' }).click();
  const drawer = page.getByRole('dialog', { name: '逐文件证据' });
  await expect(drawer).toContainText('计划源池');
  await expect(drawer).toContainText('hot');
  await expect(drawer).toContainText('8、9');
  await drawer.press('Escape');

  await page.getByRole('tab', { name: '异常明细' }).click();
  await expect(page.getByRole('row').filter({ hasText: '[0x200000401:0x66:0x0]' })).toContainText('失败');
  await expect(page.getByRole('row').filter({ hasText: '[0x200000401:0x1:0x0]' })).toHaveCount(0);
  const reason = page.getByRole('row').filter({ hasText: 'layout_swap_failed' }).filter({ hasNotText: 'lustre.pool_migrate' });
  await expect(reason).toContainText('1');
  await page.getByLabel('原因包含').fill('layout_swap');
  await page.getByRole('button', { name: '查询', exact: true }).click();
  const anomalyDownload = page.waitForEvent('download');
  await page.getByRole('link', { name: '导出异常报告' }).click();
  const anomalies = await anomalyDownload;
  expect(anomalies.suggestedFilename()).toBe(`run-${record.idExact}-exceptions.json`);
  const anomalyReport = JSON.parse(await readFile(await anomalies.path(), 'utf8'));
  expect(anomalyReport.scope.filters).toMatchObject({ exceptionsOnly: true, reasonContains: 'layout_swap' });
  expect(anomalyReport.operations).toHaveLength(1);
  expect(anomalyReport.operations[0]).toMatchObject({ fid: '[0x200000401:0x66:0x0]', state: 'failed', finishedAt: null });
  await page.getByRole('tab', { name: '报告', exact: true }).click();
  await expect(page.getByText('原因包含「layout_swap」')).toHaveCount(2);
  await expect(page.getByRole('link', { name: '导出运行报告' }))
    .toHaveAttribute('href', `/api/runs/${record.idExact}/reports/run?reason=layout_swap`);
  await expect(page.getByRole('cell', { name: '操作记录数 : 1', exact: true })).toHaveCount(2);
  await page.getByRole('tab', { name: '异常明细' }).click();
  await page.getByRole('button', { name: '重置筛选' }).click();

  await page.getByRole('tab', { name: '报告', exact: true }).click();
  await expect(page.getByText('918,734,323,983,581,286 B')).toBeVisible();
  const fullDownload = page.waitForEvent('download');
  await page.getByRole('link', { name: '导出运行报告' }).click();
  const full = await fullDownload;
  expect(full.suggestedFilename()).toBe(`run-${record.idExact}.json`);
  const report = JSON.parse(await readFile(await full.path(), 'utf8'));
  expect(report.operationResults).toMatchObject({ matchedCount: 103, pendingCount: 0, completedLogicalBytes: '918734323983581286' });
  expect(report.policyRevision.definition.metadata.name).toBe(record.policyName);
  expect(report.operations).toHaveLength(103);
  expect(report.operations.at(-1).completionEvidence).toEqual(evidence);
  await revisionLink.click();
  await expect(page.getByRole('heading', { name: record.policyName, exact: true })).toBeVisible();
  await page.getByRole('link', { name: '查看当前修订' }).click();
  await expect(page.getByRole('heading', { name: `${record.policyName}-current`, exact: true })).toBeVisible();
  await expect(page.getByRole('link', { name: `#${record.idExact}`, exact: true })).toBeVisible();
  await page.getByRole('button', { name: '查看执行历史' }).click();
  await expect(page.getByRole('link', { name: '在执行记录中查看' }))
    .toHaveAttribute('href', `#/runs?policyId=${record.policyIdExact}`);
  await expect(page.getByRole('link', { name: `#${record.idExact}`, exact: true })).toBeVisible();
});
