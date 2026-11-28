import { expect, test } from '@playwright/test';
import { controlResourceEvents } from './resource-events';

test('creates a Lustre mount even when a background read fails during editing', async ({ page }) => {
  const send = await controlResourceEvents(page);
  let registered = false;
  let managementFails = false;
  const path = '/mnt/new-fs';
  const record = {
    id: 'new-fs', name: '测试集群', path, source: '192.0.2.1@tcp:/labfs', owner: 'daemon',
    options: [], autoMount: true, desiredMounted: true, tiers: {},
  };
  await page.route('**/api/filesystems/management', (route) => {
    if (route.request().method() === 'POST') {
      expect(route.request().postDataJSON()).toEqual({
        id: record.id, name: record.name,
        mount: { type: 'managed', filesystem: 'labfs', mgsNids: '192.0.2.1@tcp', path, autoMount: true, mountNow: true },
      });
      registered = true;
      return route.fulfill({ status: 201, json: record });
    }
    if (managementFails) return route.fulfill({ status: 503, json: { error: '挂载信息暂时不可用' } });
    return route.fulfill({ json: {
      canCreateMount: true, resources: registered ? [record] : [], availableMounts: [],
    } });
  });
  await page.route('**/api/filesystems', (route) => route.fulfill({ json: registered ? [{
    id: record.id, mountPath: path, mountStatus: 'mounted', collectionStatus: 'pending',
    observedAt: null, tieringAvailable: false, capacity: { status: 'missing', sample: null },
    mount: { source: record.source, readOnly: false }, pools: null,
  }] : [] }));
  await page.goto('/console/#/resources');
  await page.getByRole('button', { name: '添加集群' }).click();
  const form = page.getByRole('dialog', { name: '添加集群' });
  await form.getByLabel('文件系统 ID').fill(record.id);
  await form.getByLabel('集群名称').fill(record.name);
  await expect(form.getByLabel('接入方式')).toHaveCount(0);
  await form.getByLabel('Lustre 文件系统名称').fill('labfs');
  await form.getByLabel('MGS NID').fill('192.0.2.1@tcp');
  await form.getByLabel('挂载路径').fill(path);
  managementFails = true;
  await send('management', '*');
  await expect(page.getByText('挂载信息暂时不可用')).toBeAttached();
  await expect(form.getByLabel('文件系统 ID')).toHaveValue(record.id);
  managementFails = false;
  await form.getByRole('button', { name: '添加', exact: true }).click();
  await expect(page.getByRole('link', { name: record.name })).toBeVisible();
});

test('shows a restored mount before collection and keeps the cluster after unmount', async ({ page }) => {
  const id = 'restored-fs';
  const name = '恢复的集群';
  const path = '/mnt/rbh-lustre/restored-fs';
  const source = '192.0.2.1@tcp:/labfs';
  let mounted = true;
  const managementReady = Promise.withResolvers<void>();
  const staleSnapshot = {
    id, mountPath: path, mountStatus: 'unmounted', mount: null, collectionStatus: 'unavailable',
    observedAt: 1795852800, tieringAvailable: false, previewAvailable: false, reason: 'not_a_lustre_mount',
    capacity: { status: 'missing', attemptedAt: 1795852800, sample: null, reason: 'not_a_lustre_mount' }, pools: null,
  };
  await page.route('**/api/filesystems', (route) => route.fulfill({ json: [staleSnapshot] }));
  await page.route('**/api/filesystems/detail?filesystem=restored-fs', (route) => route.fulfill({ json: staleSnapshot }));
  await page.route('**/api/filesystems/management', async (route) => {
    await managementReady.promise;
    await route.fulfill({ json: {
      canCreateMount: true, availableMounts: [], resources: [{
        id, name, path, source, owner: 'daemon', options: [], autoMount: true, desiredMounted: mounted, mounted, tiers: {},
      }],
    } });
  });
  await page.route('**/api/filesystems/management/restored-fs', (route) => {
    expect(route.request().method()).toBe('POST');
    expect(route.request().postDataJSON()).toEqual({ action: 'unmount' });
    mounted = false;
    return route.fulfill({ status: 204 });
  });

  const snapshot = page.waitForResponse((response) => new URL(response.url()).pathname === '/api/filesystems');
  await page.goto('/console/#/resources');
  await snapshot;
  const row = page.getByRole('row').filter({ hasText: name });
  try {
    await expect(page.getByText('正在读取', { exact: true })).toBeVisible();
    await expect(page.getByRole('row').filter({ hasText: path })).toHaveCount(0);
  } finally {
    managementReady.resolve();
  }
  await expect(row).toContainText('已挂载');
  await expect(row).toContainText('等待采集');
  await row.getByRole('link', { name }).click();
  await expect(page.getByRole('heading', { name })).toBeVisible();
  await expect(page.getByRole('button', { name: '移除接入' })).toHaveCount(0);
  await page.getByRole('button', { name: '卸载', exact: true }).click();
  await page.getByRole('dialog', { name: '卸载' }).getByRole('button', { name: '确认' }).click();
  await expect(page.getByRole('button', { name: '挂载', exact: true })).toBeVisible();
  await expect(page.getByText('未挂载', { exact: true })).toBeVisible();
  await page.getByRole('tab', { name: '挂载信息' }).click();
  await expect(page.getByText('手动挂载后启用')).toBeVisible();
  await page.getByRole('link', { name: '返回 Lustre 集群列表' }).click();
  await expect(row).toContainText('未挂载');
});

test('lists an externally discovered mount without registering it', async ({ page }) => {
  const path = '/mnt/external';
  const source = '192.0.2.1@tcp:/external';
  await page.route('**/api/filesystems', (route) => route.fulfill({ json: [] }));
  await page.route('**/api/filesystems/management', (route) => route.fulfill({ json: {
    canCreateMount: false, resources: [], availableMounts: [{ path, source }],
  } }));
  await page.goto('/console/#/resources');
  const row = page.getByRole('row').filter({ hasText: path });
  await expect(row.getByRole('cell').nth(0)).toHaveText('—');
  await expect(row.getByRole('cell').nth(1)).toHaveText(`external${source}`);
  await expect(row.getByRole('cell').nth(2)).toHaveText(path);
  await expect(row).toContainText('已挂载');
  await expect(row.getByRole('link')).toHaveCount(0);
  await expect(page.getByRole('button', { name: '添加集群' })).toHaveCount(0);
});

test('updates the open resource on notification and follows an associated policy', async ({ page }) => {
  const send = await controlResourceEvents(page);
  const now = Math.floor(Date.now() / 1000);
  const capacity = {
    status: 'collected', attemptedAt: now, reason: null,
    sample: { observedAt: now, totalBytes: 1024, usedBytes: 512, availableBytes: 256, usedRatio: 0.5 },
  };
  let newerSnapshot = false;
  let removed = false;
  let detailRequests = 0;
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
      { name: 'cold', ostIndices: [10], eligible: true, reason: null, capacity: newerSnapshot ? {
        status: 'error', attemptedAt: now + 1, sample: capacity.sample, reason: 'OST 查询失败',
      } : { status: 'missing', attemptedAt: now, sample: null, reason: '容量不可用' } },
      { name: 'aged', ostIndices: [10], eligible: true, reason: null, capacity: {
        ...capacity, sample: { ...capacity.sample, observedAt: now - 360 },
      } },
    ],
  });
  await page.route('**/api/filesystems/detail?filesystem=lab-a', (route) => {
    detailRequests += 1;
    return route.fulfill({ json: resource() });
  });
  await page.route('**/api/filesystems/detail?filesystem=lab-b', (route) => removed
    ? route.fulfill({ status: 404, json: { error: 'Lustre 文件系统 lab-b 未注册' } })
    : route.fulfill({ json: { ...resource(), id: 'lab-b', tieringAvailable: false } }));
  await page.route('**/api/filesystems/management', (route) => route.fulfill({ json: {
    canCreateMount: false, resources: [], availableMounts: [],
  } }));
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

  await expect(page.getByRole('button', { name: '刷新', exact: true })).toHaveCount(0);
  const previousReads = detailRequests;
  const mountsRead = page.waitForResponse((response) => new URL(response.url()).pathname === '/api/filesystems/management');
  newerSnapshot = true;
  await send('snapshot', 'lab-b');
  await send('management', '*');
  await mountsRead;
  await expect(pool('cold')).toContainText('未取得容量');
  await send('snapshot', 'lab-a');
  await expect(pool('cold')).toContainText('容量已过期');
  await expect(pool('cold')).toContainText('OST 查询失败');
  await expect(pool('cold')).toContainText('1 KiB');
  expect(listRequests).toBe(0);
  expect(detailRequests).toBe(previousReads + 1);
  await expect(page.getByRole('tab', { name: '存储资源' })).toHaveAttribute('aria-selected', 'true');

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
  await page.getByRole('option', { name: '分层不可用', exact: true }).click();
  await expect(labRow).toHaveCount(0);
  await expect(page.getByText('共 1 / 2 个资源')).toBeVisible();
  await page.getByRole('row').filter({ hasText: 'lab-b' }).getByRole('link', { name: '详情' }).click();
  await expect(page.getByRole('heading', { name: 'lab-b', exact: true })).toBeVisible();
  removed = true;
  await send('management', 'lab-b');
  await expect(page.getByText('Lustre 文件系统未注册', { exact: true })).toBeVisible();
});
