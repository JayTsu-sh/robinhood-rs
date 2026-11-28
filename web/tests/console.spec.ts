import { expect, test } from '@playwright/test';

test('the daemon serves the compiled console and its real read APIs', async ({ page, request }) => {
  const [policyResponse, resourceResponse] = await Promise.all([
    request.get('/api/policies?configuredOnly=true'),
    request.get('/api/filesystems'),
  ]);
  expect(policyResponse.ok()).toBe(true);
  expect(resourceResponse.ok()).toBe(true);
  const resources: {
    id: string;
    mountPath: string;
    pools: { name: string; eligible: boolean }[] | null;
  }[] = await resourceResponse.json();
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
});

test('policies show saved values and distinguish empty, filtered, and failed requests', async ({ page }) => {
  let status = 200;
  let revision = 7;
  let seen = true;
  await page.route('**/api/policies?configuredOnly=true', (route) =>
    route.fulfill({
      status,
      json: status !== 200 ? { error: '数据库不可用' } : seen ? [
        {
          id: 42,
          revision,
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
  await expect(page.getByText('原生归档')).toHaveCount(0);
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
  await page.route('**/api/filesystems', (route) => route.fulfill({
    json: [{
      id: 'lab-a',
      mountPath: '/mnt/lab',
      mountStatus: mountMissing ? 'unmounted' : 'mounted',
      collectionStatus: mountMissing ? 'unavailable' : 'partial',
      observedAt: 1795852800,
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
  mountMissing = true;
  await page.getByRole('button', { name: '刷新' }).click();
  await expect(row).toContainText('未挂载');
  await expect(row).toContainText('无法确认可选池');
  await expect(row).toContainText('配置路径未挂载 Lustre');
});
