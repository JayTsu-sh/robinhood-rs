import { expect, test } from '@playwright/test';
import type { FilesystemResource } from '../src/services/tiering';

test('the daemon serves the compiled console and its real read APIs', { tag: '@daemon' }, async ({ page, request }) => {
  const [policyResponse, resourceResponse] = await Promise.all([
    request.get('/api/policies?configuredOnly=true'),
    request.get('/api/filesystems'),
  ]);
  expect(policyResponse.ok()).toBe(true);
  expect(resourceResponse.ok()).toBe(true);
  const resources: FilesystemResource[] = await resourceResponse.json();
  const policies: {
    id: number;
    revision: number;
    definition: {
      metadata: { name: string };
      spec: {
        enabled?: boolean;
        target: { filesystem: string };
        rules: {
          entries: {
            decision: string;
            action?: { type: string; params?: { sourcePool: string; targetPool: string } };
          }[];
        };
      };
    };
  }[] = await policyResponse.json();
  await page.goto('/console/');
  await expect(page.getByRole('heading', { name: '分层策略' })).toBeVisible();
  const sample = policies.find(({ definition }) => definition.spec.rules.entries.some(
    (rule) => rule.decision !== 'skip' && rule.action?.type === 'lustre.pool_migrate' &&
      rule.action.params?.sourcePool && rule.action.params?.targetPool,
  ));
  if (process.env.RBH_CONSOLE_REQUIRE_LIVE_DATA === '1') {
    expect(sample, 'the daemon must have a saved policy with an explicit pool pair').toBeDefined();
    expect(resources.some(({ pools }) => (pools?.filter(({ eligible }) => eligible).length ?? 0) >= 2),
      'the daemon must report two eligible pools on a Lustre filesystem').toBe(true);
  }
  if (sample) {
    const { id, revision, definition: { metadata, spec } } = sample;
    await page.getByRole('searchbox', { name: '搜索策略' }).fill(metadata.name);
    const row = page.getByRole('row').filter({ has: page.getByRole('cell', { name: metadata.name, exact: true }) });
    const pair = spec.rules.entries.find((rule) => rule.action?.type === 'lustre.pool_migrate' &&
      rule.action.params?.sourcePool && rule.action.params?.targetPool)?.action?.params;
    await expect(row).toContainText(`${pair?.sourcePool} → ${pair?.targetPool}`);
    await expect(row).toContainText(spec.enabled === false ? '已停用' : '已启用');
    await expect(row.getByRole('cell').nth(1)).toHaveText(String(id));
    await expect(row.getByRole('cell').nth(2)).toHaveText(String(revision));
  }
  await page.getByRole('link', { name: 'Lustre 集群' }).click();
  await page.reload();
  await expect(page.getByRole('heading', { name: 'Lustre 集群' })).toBeVisible();
  for (const resource of resources) {
    const row = page.getByRole('row').filter({ has: page.getByRole('cell', { name: resource.id, exact: true }) });
    await expect(row).toContainText(resource.mountPath);
    for (const pool of resource.pools?.filter(({ eligible }) => eligible) ?? []) {
      await expect(row.getByText(pool.name, { exact: true })).toBeVisible();
    }
  }
  if (!resources.length) {
    await expect(page.getByText('当前 daemon 未注册 Lustre 文件系统')).toBeVisible();
  }
  const mounted = resources.find(({ tieringAvailable }) => tieringAvailable);
  if (mounted) {
    await page.getByRole('link', { name: mounted.id, exact: true }).click();
    await page.getByRole('tab', { name: '存储资源' }).click();
    const pool = mounted.pools?.find(({ eligible }) => eligible);
    if (pool) {
      await expect(page.getByRole('cell', { name: pool.name, exact: true })).toBeVisible();
    }
    await page.getByRole('tab', { name: '挂载信息' }).click();
    await page.reload();
    await expect(page.getByRole('heading', { name: mounted.id, exact: true })).toBeVisible();
    await expect(page.getByText(mounted.mount?.source ?? '未取得', { exact: true })).toBeVisible();
  }
});

test('policies show saved values and distinguish empty, filtered, and failed requests', async ({ page }) => {
  let status = 200;
  let revision = 7;
  let seen = true;
  await page.route('**/api/policies?configuredOnly=true&includeLatestRun=true', (route) =>
    route.fulfill({
      status,
      json: status !== 200 ? { error: '数据库不可用' } : seen ? [
        {
          id: 42,
          revision,
          idExact: '42',
          revisionExact: String(revision),
          latestRun: { id: '501', revision: '6', state: 'running', startedAt: '2026-11-28T06:15:00Z' },
          definition: {
            metadata: { name: '按访问时间降温' },
            spec: {
              enabled: false,
              target: { filesystem: 'lab-a' },
              rules: {
                defaults: { action: { type: 'lustre.pool_migrate', params: { sourcePool: 'hot', targetPool: 'warm' } } },
                entries: [
                  { decision: 'skip' },
                  { decision: 'execute', action: { params: { targetPool: 'cold' } } },
                ],
              },
            },
          },
        },
        {
          id: 5,
          revision: 1,
          idExact: '5',
          revisionExact: '1',
          definition: {
            metadata: { name: '原生归档' },
            spec: {
              target: { filesystem: 'lab-a' },
              rules: { entries: [{ decision: 'execute', action: { type: 'lustre.hsm.archive' } }] },
            },
          },
        },
      ] : [],
    }),
  );
  await page.goto('/console/#/policies');
  const row = page.getByRole('row').filter({ hasText: '按访问时间降温' });
  await expect(row).toContainText('42');
  await expect(row).toContainText('7');
  await expect(row).toContainText('lab-a');
  await expect(row).toContainText('hot → cold');
  await expect(row).toContainText('已停用');
  await expect(row).toContainText('派发中');
  await expect(row.getByRole('link', { name: '#501' })).toHaveAttribute('href', '#/runs/501');
  await expect(page.getByText('原生归档')).toHaveCount(0);
  const runState = page.getByRole('combobox', { name: '当前或最近运行状态' });
  await runState.press('ArrowDown');
  const options = page.locator(`[id="${await runState.getAttribute('aria-controls')}"]`);
  await options.getByRole('option', { name: '未执行' }).click();
  await expect(page.getByText('没有符合筛选条件的策略')).toBeVisible();
  await page.getByRole('button', { name: '重置筛选' }).click();
  await page.getByRole('searchbox', { name: '搜索策略' }).fill('无匹配');
  await expect(page.getByText('没有符合筛选条件的策略')).toBeVisible();
  await page.getByRole('button', { name: '重置筛选' }).click();
  revision = 8;
  await page.getByRole('button', { name: '刷新' }).click();
  await expect(row).toContainText('8');
  status = 503;
  await page.getByRole('button', { name: '刷新' }).click();
  await expect(page.getByRole('alert')).toContainText('数据库不可用');
  status = 200;
  seen = false;
  await page.getByRole('button', { name: '重试' }).click();
  await expect(page.getByText('暂无分层策略')).toBeVisible();
});

test('resources show eligible pools, failures and fresh results independently of policy data', async ({ page }) => {
  let mountMissing = false;
  await page.route('**/api/filesystems/refresh', (route) => {
    expect(route.request().method()).toBe('POST');
    mountMissing = true;
    return route.fulfill({ status: 204 });
  });
  await page.route('**/api/filesystems', (route) => route.fulfill({
    json: [{
      id: 'lab-a',
      mountPath: '/mnt/lab',
      mountStatus: mountMissing ? 'unmounted' : 'mounted',
      collectionStatus: mountMissing ? 'unavailable' : 'partial',
      observedAt: 1795852800,
      mount: mountMissing ? null : { source: '10.1.2.3@tcp:/lab', readOnly: false },
      capacity: { status: 'missing', attemptedAt: 1795852800, sample: null, reason: null },
      pools: mountMissing ? null : [
        { name: 'cold', eligible: true, ostIndices: [8], reason: null },
        { name: 'empty', eligible: false, ostIndices: [], reason: 'pool_has_no_osts' },
        { name: 'hot', eligible: true, ostIndices: [1, 2], reason: null },
        { name: 'unknown', eligible: false, ostIndices: null, reason: '采集失败' },
      ],
      tieringAvailable: !mountMissing,
      reason: mountMissing ? 'not_a_lustre_mount' : null,
    }],
  }));
  await page.goto('/console/#/resources');
  const row = page.getByRole('row').filter({ hasText: 'lab-a' });
  await expect(row).toContainText('/mnt/lab');
  await expect(row).toContainText('可用于分层');
  await expect(row).toContainText('cold');
  await expect(row).toContainText('hot');
  await expect(row.getByText('unknown', { exact: true })).toHaveCount(0);
  await expect(row).toContainText('部分采集');
  await row.getByRole('button', { name: '查看原因' }).click();
  await expect(page.getByText('unknown · 采集失败')).toBeVisible();
  await page.getByRole('button', { name: '刷新' }).click();
  await expect(row).toContainText('未挂载');
  await expect(row).toContainText('无法确认可选池');
  await expect(row).toContainText('配置路径未挂载 Lustre');
});

test('an unsaved tiering preview uses the selected pool pair and invalidates old results', async ({ page }) => {
  const pools = ['hot', 'cold', 'empty'].map((name) => ({ name, eligible: name !== 'empty' }));
  await page.route('**/api/filesystems', (route) => route.fulfill({
    json: [
      { id: 'lab-a', pools, tieringAvailable: true, previewAvailable: true },
      { id: 'lab-b', pools, tieringAvailable: false, previewAvailable: true },
      { id: 'lab-offline', pools: null, tieringAvailable: false, previewAvailable: false },
    ],
  }));
  let fail = false;
  await page.route('**/api/policies/preview', async (route) => {
    expect(route.request().method()).toBe('POST');
    const { spec } = route.request().postDataJSON();
    expect(spec.enabled).toBe(false);
    expect(spec.target).toMatchObject({
      filesystem: 'lab-a',
      directoryScope: { root: '/team', excludedSubtrees: ['/team/archive'] },
    });
    expect(spec.rules.entries[0].action.params).toMatchObject({ sourcePool: 'hot', targetPool: 'cold' });
    if (fail) {
      await route.fulfill({ status: 400, json: { error: 'targetPool 不可用' } });
      return;
    }
    await route.fulfill({
      json: {
        revision: null,
        candidateCount: 2,
        estimatedBytes: 9007199254740992,
        estimatedBytesExact: "9007199254740993",
        resourceBindings: [{ kind: 'ExecutionProfile', name: 'builtin-preview', revision: 3 }],
        directoryScope: {
          excludedByReason: { link_outside_scope: 4, link_in_excluded_subtree: 5 },
          deferredByReason: { incomplete_link_evidence: 6 },
        },
      },
    });
  });

  async function openChoices(name: string) {
    const select = page.getByRole('combobox', { name, exact: true });
    await select.press('ArrowDown');
    const listId = await select.getAttribute('aria-controls');
    return page.locator(`[id="${listId}"]`);
  }

  await page.goto('/console/#/policies');
  await page.getByRole('link', { name: '预览迁移' }).click();
  await expect(page.getByRole('heading', { name: '预览池迁移' })).toBeVisible();
  const filesystemChoices = await openChoices('Lustre 文件系统');
  await expect(filesystemChoices.getByRole('option', { name: 'lab-offline' })).toHaveCount(0);
  await filesystemChoices.getByRole('option', { name: 'lab-a' }).click();
  const sourceChoices = await openChoices('源池');
  await expect(sourceChoices.getByRole('option', { name: 'empty' })).toHaveCount(0);
  await sourceChoices.getByRole('option', { name: 'hot' }).click();
  const targetChoices = await openChoices('目标池');
  await expect(targetChoices.getByRole('option', { name: 'hot' })).toHaveCount(0);
  await targetChoices.getByRole('option', { name: 'cold' }).click();
  await page.getByLabel('起始目录').fill('/team');
  await page.getByRole('button', { name: '添加排除目录' }).click();
  await page.getByRole('textbox', { name: '排除子目录 1', exact: true }).fill('/team/archive');
  await page.getByRole('button', { name: '计算预览' }).click();

  const result = page.getByRole('region', { name: '预览结果' });
  await expect(result).toContainText('9,007,199,254,740,993');
  await expect(result).toContainText('链接位于范围外');
  await expect(result).toContainText('链接位于排除子树');
  await expect(result).toContainText('链接证据不完整');
  await expect(result).toContainText('builtin-preview');
  await page.getByLabel('起始目录').fill('/team-new');
  await expect(result).toHaveCount(0);

  fail = true;
  await page.getByLabel('起始目录').fill('/team');
  await page.getByRole('button', { name: '计算预览' }).click();
  await expect(page.getByRole('alert')).toContainText('targetPool 不可用');
  await expect(page.getByRole('textbox', { name: '排除子目录 1', exact: true })).toHaveValue('/team/archive');

  await (await openChoices('Lustre 文件系统')).getByRole('option', { name: 'lab-b' }).click();
  await expect(page.getByText('预览失败')).toHaveCount(0);
  await expect(page.getByText('选择源池', { exact: true })).toBeVisible();
  await expect(page.getByText('选择目标池', { exact: true })).toBeVisible();
});
