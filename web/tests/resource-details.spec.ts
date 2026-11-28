import { expect, test } from '@playwright/test';

test('opens a resource directly, refreshes that resource and follows an associated policy', async ({ page }) => {
  const now = Math.floor(Date.now() / 1000);
  const capacity = {
    status: 'collected', attemptedAt: now, reason: null,
    sample: { observedAt: now, totalBytes: 1024, usedBytes: 512, availableBytes: 256, usedRatio: 0.5 },
  };
  let refresh = false;
  let listRequests = 0;
  const resource = () => ({
    id: 'lab-a', mountPath: '/mnt/lab', mountStatus: 'mounted', collectionStatus: 'collected',
    observedAt: now, tieringAvailable: true, reason: null, capacity,
    mount: { source: '10.1.2.3@tcp:/lustre', readOnly: false },
    pools: [
      { name: 'hot', ostIndices: [0, 10], eligible: true, reason: null, capacity },
      { name: 'empty', ostIndices: [], eligible: false, reason: 'pool_has_no_osts', capacity: {
        ...capacity, sample: { ...capacity.sample, totalBytes: 0, usedBytes: 0, availableBytes: 0, usedRatio: 0 },
      } },
      { name: 'incomplete', ostIndices: [6, 7], eligible: true, reason: null, capacity: {
        status: 'partial', attemptedAt: now, sample: null, reason: '仅取得 1/2 个 OST 的容量',
      } },
      { name: 'cold', ostIndices: [10], eligible: true, reason: null, capacity: refresh ? {
        status: 'error', attemptedAt: now + 1, sample: capacity.sample, reason: 'OST 查询失败',
      } : { status: 'missing', attemptedAt: now, sample: null, reason: '容量不可用' } },
      { name: 'aged', ostIndices: [10], eligible: true, reason: null, capacity: {
        ...capacity, sample: { ...capacity.sample, observedAt: now - 360 },
      } },
    ],
  });
  await page.route('**/api/filesystems/detail?filesystem=lab-a', (route) => route.fulfill({ json: resource() }));
  await page.route('**/api/filesystems/detail?filesystem=lab-b', (route) => route.fulfill({
    json: { ...resource(), id: 'lab-b', tieringAvailable: false },
  }));
  let resourceRefreshes = 0;
  await page.route('**/api/filesystems/refresh?filesystem=lab-a', (route) => {
    expect(route.request().method()).toBe('POST');
    resourceRefreshes += 1;
    refresh = true;
    return route.fulfill({ status: 204 });
  });
  await page.route('**/api/filesystems', (route) => {
    listRequests += 1;
    return route.fulfill({ json: [resource(), { ...resource(), id: 'lab-b', tieringAvailable: false }] });
  });
  await page.route((url) => url.pathname === '/api/policies' && url.searchParams.get('configuredOnly') === 'true', (route) => route.fulfill({
    json: ['lab-a', 'lab-b'].map((filesystem, index) => ({
      id: index + 42, revision: 1,
      idExact: String(index + 42), revisionExact: '1',
      definition: {
        metadata: { name: `数据降温 ${filesystem}` },
        spec: { enabled: false, target: { filesystem }, rules: { entries: [
          { decision: 'execute', action: { type: 'lustre.pool_migrate', params: { sourcePool: 'hot', targetPool: 'cold' } } },
        ] } },
      },
    })),
  }));

  await page.goto('/console/#/resources/detail?filesystem=lab-a&tab=storage');
  await page.reload();
  await expect(page.getByRole('heading', { name: 'lab-a', exact: true })).toBeVisible();
  const pool = (name: string) => page.getByRole('row').filter({ has: page.getByRole('cell', { name, exact: true }) });
  await expect(pool('hot')).toContainText('OST000a');
  await expect(pool('hot')).toContainText('1 KiB');
  await expect(pool('hot')).toContainText('50%');
  await expect(pool('empty')).toContainText('0 B');
  await expect(pool('incomplete')).toContainText('部分缺失');
  await expect(pool('incomplete')).toContainText('最近尝试');
  await expect(pool('cold')).toContainText('未取得容量');
  await expect(pool('aged')).toContainText('容量已过期');

  expect(resourceRefreshes).toBe(0);
  await page.getByRole('button', { name: '刷新' }).click();
  await expect(pool('cold')).toContainText('容量已过期');
  await expect(pool('cold')).toContainText('OST 查询失败');
  await expect(pool('cold')).toContainText('1 KiB');
  expect(listRequests).toBe(0);
  expect(resourceRefreshes).toBe(1);

  await page.getByRole('tab', { name: '挂载信息' }).click();
  await expect(page.getByText('10.1.2.3@tcp:/lustre')).toBeVisible();
  await expect(page.getByText('读写挂载')).toBeVisible();
  await page.getByRole('tab', { name: '集群概览' }).click();
  await page.getByRole('link', { name: '数据降温 lab-a' }).click();
  await expect(page.getByRole('row').filter({ hasText: '数据降温 lab-a' })).toBeVisible();
  await expect(page.getByRole('row').filter({ hasText: '数据降温 lab-b' })).toHaveCount(0);

  await page.getByRole('link', { name: 'Lustre 集群' }).click();
  const labRow = page.getByRole('row').filter({ hasText: 'lab-a' });
  await expect(labRow).toContainText('1 KiB');
  await page.getByRole('combobox', { name: '分层可用性' }).press('Enter');
  await page.getByRole('option', { name: '暂不可用于分层', exact: true }).click();
  await expect(labRow).toHaveCount(0);
  await page.getByRole('row').filter({ hasText: 'lab-b' }).getByRole('cell', { name: '/mnt/lab' }).click();
  await expect(page.getByRole('heading', { name: 'lab-b', exact: true })).toBeVisible();
});
