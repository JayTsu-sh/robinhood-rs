import { expect, test, type Locator, type Page } from '@playwright/test';

test.use({ actionTimeout: 8_000, navigationTimeout: 12_000 });
const token = process.env.RBH_CONSOLE_LUSTRE_TEST_TOKEN;
const mountSource = process.env.RBH_CONSOLE_LUSTRE_MOUNT_SOURCE;
const separator = mountSource?.lastIndexOf(':/') ?? -1;
const managementServers = mountSource?.slice(0, separator);
const filesystem = mountSource?.slice(separator + 2);
const id = 'qa-managed';
const label = '容器验收集群';
const renamed = '容器验收改名';
const api = '/api/filesystems/management';
const select = async (dialog: Locator, label: string, option: string | RegExp, page: Page) => {
  await dialog.getByRole('combobox', { name: new RegExp(label + "$") }).press("ArrowDown");
  await page.getByRole('option', { name: option, exact: true }).click();
};
async function confirm(dialog: Locator, button: string, close = true) {
  await dialog.getByRole('button', { name: new RegExp(button + "$") }).click();
  await dialog.getByLabel('写入令牌').fill(token!);
  await dialog.getByRole('button', { name: '验证并继续' }).click();
  if (close) await expect(dialog).toBeHidden({ timeout: 40_000 });
}

test('creates and manages a real Lustre mount through the GUI', { tag: '@daemon' }, async ({ page, request }, testInfo) => {
  test.skip(!mountSource || !token, 'requires the container Lustre fixture');
  expect(separator).toBeGreaterThan(0);
  test.setTimeout(120_000);
  await page.goto('/console/#/resources');
  await page.getByRole('button', { name: '添加集群' }).click();
  const add = page.getByRole('dialog', { name: '添加集群' });
  await expect(add.getByLabel('接入方式')).toHaveCount(0);
  await add.getByLabel('文件系统 ID').fill(id);
  await add.getByLabel('集群名称').fill(label);
  await add.getByLabel('Lustre 文件系统名称').fill(filesystem!);
  await add.getByLabel('MGS NID').fill(managementServers!);
  await confirm(add, '测试连接', false);
  await expect(add.getByText('MGS NID 可达。', { exact: false })).toBeVisible();
  await confirm(add, '添加');
  const cluster = page.getByRole('link', { name: label, exact: true });
  const row = page.getByRole('row').filter({ has: cluster });
  await expect(row.getByRole('cell', { name: `/mnt/${id}`, exact: true })).toBeVisible();
  await cluster.click();
  await expect(page.getByRole('heading', { name: new RegExp(label + "$") })).toBeVisible();
  await expect(page.getByRole('button', { name: '卸载', exact: true })).toBeVisible();
  await expect.poll(async () => {
    const collections = await (await request.get('/api/filesystems')).json();
    return collections.find((item: { id: string }) => item.id === id)?.tieringAvailable;
  }).toBe(true);
  await page.getByRole('button', { name: '编辑集群名称' }).click();
  const rename = page.getByRole('dialog', { name: '编辑集群名称' });
  await rename.getByLabel('集群名称').fill(renamed);
  await confirm(rename, '确认');
  await expect(page.getByRole('heading', { name: renamed, exact: true })).toBeVisible();
  await page.getByRole('button', { name: '编辑 Pool 层级' }).click();
  const tier = page.getByRole('dialog', { name: '编辑 Pool 层级' });
  await select(tier, 'Pool', 'cold', page);
  await select(tier, '应用侧层级标签', '冷层', page);
  await confirm(tier, '确认');
  await page.getByRole('tab', { name: '存储资源' }).click();
  await expect(page.getByRole('row').filter({ has: page.getByRole('cell', { name: 'cold', exact: true }) }))
    .toContainText('冷层');
  await page.getByRole('button', { name: '设置自动恢复' }).click();
  const auto = page.getByRole('dialog', { name: '设置自动恢复' });
  await auto.getByRole('checkbox').uncheck();
  await confirm(auto, '确认');
  await expect.poll(async () => (await (await request.get(api)).json()).resources[0]?.autoMount).toBe(false);
  await page.getByRole('button', { name: '设置自动恢复' }).click();
  await auto.getByRole('checkbox').check();
  await confirm(auto, '确认');
  await expect.poll(async () => (await (await request.get(api)).json()).resources[0]?.autoMount).toBe(true);
  await page.getByRole('tab', { name: '挂载信息' }).click();
  await expect(page.getByText(mountSource!, { exact: true })).toBeVisible();
  await page.screenshot({ path: testInfo.outputPath('managed-mount.png'), fullPage: true });
});

test('restores after container restart and keeps the cluster after unmount', { tag: '@daemon' }, async ({ page, request }) => {
  test.skip(!mountSource || !token, 'requires the container Lustre fixture');
  test.setTimeout(120_000);
  await page.goto('/console/#/resources');
  const row = page.getByRole('row').filter({ has: page.getByRole('link', { name: renamed, exact: true }) });
  await expect(row).toContainText('已挂载');
  await row.getByRole('link', { name: renamed, exact: true }).click();
  await expect(page.getByRole('heading', { name: renamed, exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: '卸载', exact: true })).toBeVisible();
  await page.getByRole('tab', { name: '存储资源' }).click();
  await expect(page.getByRole('row').filter({ has: page.getByRole('cell', { name: 'cold', exact: true }) }))
    .toContainText('冷层');
  await page.getByRole('button', { name: '卸载', exact: true }).click();
  await confirm(page.getByRole('dialog', { name: '卸载' }), '确认');
  await page.getByRole('button', { name: '挂载', exact: true }).click();
  await confirm(page.getByRole('dialog', { name: '挂载' }), '确认');
  await expect(page.getByRole('button', { name: '卸载', exact: true })).toBeVisible();
  await page.getByRole('button', { name: '卸载', exact: true }).click();
  await confirm(page.getByRole('dialog', { name: '卸载' }), '确认');
  await expect(page.getByRole('button', { name: '移除接入' })).toHaveCount(0);
  await expect(page.getByRole('button', { name: '挂载', exact: true })).toBeVisible();
  await expect.poll(async () => (await (await request.get(api)).json()).resources
    .find((item: { id: string }) => item.id === id)).toMatchObject({ mounted: false, desiredMounted: false, autoMount: true });
  await page.getByRole('link', { name: '返回 Lustre 集群列表' }).click();
  await expect(row).toContainText('未挂载');
});

test('shows an actual external mount directly in the cluster list', { tag: '@daemon' }, async ({ page }) => {
  test.skip(!mountSource || !token, 'requires the container Lustre fixture');
  test.setTimeout(90_000);
  await page.goto('/console/#/resources');
  const row = page.getByRole('row').filter({ hasText: '/mnt/qa-existing' });
  await expect(row).toContainText(mountSource!);
  await expect(row).toContainText('已挂载');
  await expect(row.getByRole('link')).toHaveCount(0);
});

test('authenticates pause, resume, cancel and approval against MariaDB', { tag: '@daemon' }, async ({ page, request }) => {
  test.setTimeout(50_000);
  test.skip(!token, 'requires the container Lustre fixture');
  const open = async (id: string) => page.goto('/console/#/runs/' + id);
  const control = async (page: Page, label: string) => {
    await page.getByRole('button', { name: label, exact: true }).click();
    const dialog = page.getByRole('dialog', { name: label, exact: true });
    const submit = dialog.getByRole('button', { name: new RegExp(label + '$') });
    await submit.click();
    await dialog.getByLabel('策略写入令牌').fill(token!);
    await submit.click();
    await expect(dialog).toBeHidden();
  };
  const findRun = async (state: string) => {
    const result = await request.get('/api/runs?policyName=qa-run-controls&state=' + state);
    expect(result.ok()).toBe(true);
    const { items } = await result.json();
    expect(items).toHaveLength(1);
    return items[0].idExact as string;
  };
  const assertState = async (id: string, state: string) => {
    await expect.poll(async () => (await (await request.get('/api/runs/' + id)).json()).state).toBe(state);
  };
  const queued = await findRun('queued');
  await open(queued);
  await control(page, '暂停');
  await assertState(queued, 'paused');
  await expect(page.getByText('操作员暂停（qa-operator）', { exact: false })).toBeVisible();
  await control(page, '恢复');
  await assertState(queued, 'queued');
  await expect(page.getByText('操作员恢复（qa-operator）', { exact: false })).toBeVisible();
  await control(page, '取消执行');
  await assertState(queued, 'cancelled');
  await expect(page.getByText('操作员取消（qa-operator）', { exact: false })).toBeVisible();

  const pending = await findRun('awaiting_approval');
  await open(pending);
  await control(page, '批准本次运行');
  await assertState(pending, 'running');
  await expect(page.getByText('操作员批准（qa-operator）', { exact: false })).toBeVisible();
  const run = (await (await request.get('/api/runs/' + pending)).json());
  expect(run.approvedBy).toBe('qa-operator');
});
