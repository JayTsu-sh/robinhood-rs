import { expect, test, type Page } from '@playwright/test';

async function select(page: Page, label: string, option: string) {
  const control = page.getByLabel(label, { exact: false }).and(page.getByRole('combobox'));
  await control.press('ArrowDown');
  const list = page.locator(`[id="${await control.getAttribute('aria-controls')}"]`);
  const escaped = option.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
  await list.getByRole('option', { name: new RegExp(`^${escaped}(?:$| (?:可用|容量))`) }).click();
}

test('a tiering draft can be resumed and revalidated after changing the filesystem', async ({ page }) => {
  await page.route('**/api/filesystems', (route) => route.fulfill({
    json: ['lab-a', 'lab-b'].map((id) => ({
      id, tieringAvailable: true,
      pools: ['hot', 'cold'].map((name) => ({ name, eligible: true })),
    })),
  }));
  for (const resource of ['execution-profiles', 'authorization-policies', 'approval-policies']) {
    await page.route(`**/api/${resource}`, (route) => route.fulfill({
      json: [{ name: 'builtin-preview', revision: 1 }],
    }));
  }
  await page.goto('/console/#/policies');
  await page.getByRole('link', { name: '新建策略' }).click();
  await expect(page.getByRole('heading', { name: '新建策略' })).toBeVisible();
  await page.getByRole('button', { name: '下一步' }).click();
  await expect(page.getByText('请输入策略名称')).toBeVisible();
  await page.getByLabel('策略名称').fill('daily-tiering');
  await select(page, 'Lustre 文件系统', 'lab-a');
  await select(page, '源池', 'hot');
  await select(page, '目标池', 'cold');
  await page.getByRole('button', { name: '下一步' }).click();
  await page.getByLabel('起始目录').fill('/team');
  await page.getByRole('button', { name: '添加排除目录' }).click();
  await page.getByLabel('排除子目录 1').fill('/team/archive');
  await page.getByRole('button', { name: '保存草稿' }).click();
  await expect(page.getByText('草稿已保存到当前浏览器')).toBeVisible();
  page.once('dialog', (dialog) => dialog.dismiss());
  await page.getByRole('link', { name: 'Lustre 集群' }).click();
  await expect(page.getByRole('heading', { name: '新建策略' })).toBeVisible();
  page.once('dialog', (dialog) => dialog.accept());
  await page.getByRole('link', { name: 'Lustre 集群' }).click();
  await expect(page.getByRole('heading', { name: 'Lustre 集群' })).toBeVisible();
  await page.goto('/console/#/policies/new');
  await page.getByRole('button', { name: '恢复草稿' }).click();
  await expect(page.getByLabel('策略名称')).toHaveValue('daily-tiering');
  await page.getByRole('button', { name: '下一步' }).click();
  await expect(page.getByLabel('排除子目录 1')).toHaveValue('/team/archive');
  await page.getByRole('button', { name: '上一步' }).click();
  await select(page, 'Lustre 文件系统', 'lab-b');
  await expect(page.getByText('选择源池', { exact: true })).toBeVisible();
  await expect(page.getByText('选择目标池', { exact: true })).toBeVisible();
  await page.getByRole('button', { name: '下一步' }).click();
  await expect(page.getByText('请选择有效源池')).toBeVisible();
  page.once('dialog', (dialog) => dialog.accept());
  await page.getByRole('button', { name: '重置表单' }).click();
  await expect(page.getByLabel('策略名称')).toHaveValue('');
  await expect(page.getByRole('button', { name: '恢复草稿' })).toHaveCount(0);
});

test('the compiled wizard previews and saves a real disabled revision, with exact read-only export and edit conflicts', async ({ page, request, baseURL }) => {
  const token = process.env.RBH_CONSOLE_WIZARD_TEST_TOKEN;
  const filesystem = process.env.RBH_CONSOLE_WIZARD_TEST_FS;
  test.skip(!token || !filesystem, 'requires the isolated writer, indexed fixture, and pools described in tests/README.md');
  test.setTimeout(90_000);
  const origin = new URL(baseURL!);
  expect(origin.protocol === 'https:' || ['localhost', '127.0.0.1', '[::1]'].includes(origin.hostname)).toBe(true);
  const writer = { headers: { Authorization: `Bearer ${token}` }, maxRedirects: 0 };
  const name = `wizard-${Date.now()}`;
  const next = () => page.getByRole('button', { name: '下一步' }).click();
  const previous = () => page.getByRole('button', { name: '上一步' }).click();
  async function saveWithWriter() {
    await page.getByRole('button', { name: '保存策略' }).click();
    const writer = page.getByRole('dialog', { name: '策略写入凭据' });
    await writer.getByLabel('策略写入令牌').fill(token!);
    await writer.getByRole('button', { name: '提交保存' }).click();
  }
  const result = page.getByRole('region', { name: '预览结果' });
  async function preview(expected: number) {
    await page.getByRole('button', { name: '计算预览' }).click();
    await expect(result.locator('.ant-statistic-content').first()).toHaveText(String(expected));
  }

  await page.goto('/console/#/policies/new');
  await page.getByLabel('策略名称').fill(name);
  await select(page, 'Lustre 文件系统', filesystem!);
  await select(page, '源池', 'hot');
  await select(page, '目标池', 'cold');
  await next();
  await page.getByLabel('起始目录').fill('/team');
  await page.getByRole('button', { name: '添加排除目录' }).click();
  await page.getByLabel('排除子目录 1').fill('/team/archive');
  await page.getByRole('button', { name: '添加条件' }).last().click();
  await select(page, '比较', '=');
  await select(page, '单位', 'B');
  await page.getByLabel('数值').fill('200');
  await next();
  await page.getByLabel('规则名称 1').fill('保留优先');
  await select(page, '动作 1', '保留');
  await page.getByRole('button', { name: '添加规则' }).click();
  await page.getByLabel('规则名称 2').fill('迁移候选');
  await page.getByRole('button', { name: '添加条件' }).last().click();
  await select(page, '文件属性', '最后修改时间');
  await select(page, '单位', '秒');
  await page.getByLabel('数值').fill('1');
  await next();
  await page.getByLabel('限制每日准入窗口').check();
  await page.getByLabel('结束时刻').fill('22:00');
  await next();
  await expect(page.getByText('结束时刻须有效且不同于开始时刻')).toBeVisible();
  await page.getByLabel('结束时刻').fill('06:00');
  await next();
  await page.getByLabel('单轮最大操作数').fill('11');
  await next();
  await expect(page.getByText(/22:00–06:00.*跨午夜/)).toBeVisible();
  await preview(0);

  await previous();
  await previous();
  await previous();
  await page.getByRole('button', { name: '上移规则 2' }).click();
  await expect(page.getByLabel('规则名称 1')).toHaveValue('迁移候选');
  await expect(page.getByText('尚无已确认配置')).toHaveCount(0);
  await expect(result).toHaveCount(0);
  await next();
  await next();
  await next();
  await expect(result).toHaveCount(0);
  await preview(1);
  expect((await (await request.get('/api/policies')).json()).some(
    ({ definition }: { definition: { metadata: { name: string } } }) => definition.metadata.name === name,
  )).toBe(false);

  await saveWithWriter();
  await expect(page.getByRole('heading', { name, exact: true })).toBeVisible();
  await expect(page.getByText('已停用', { exact: true })).toBeVisible();
  await expect(page.getByRole('button', { name: '启用策略' })).toBeVisible();
  await expect(page.getByRole('button', { name: '立即运行' })).toBeDisabled();
  const pathMatch = /\/policies\/(\d+)$/.exec(page.url());
  expect(pathMatch).not.toBeNull();
  const id = pathMatch![1];
  const path = `/api/policies/${id}`;
  const current = await (await request.get(path)).json();
  expect(current.definition.spec.enabled).toBe(false);
  expect(current.definition.spec.execution.budget.run.maxOperations).toBe(11);
  expect(current.definition.spec.rules.entries.map(({ name }: { name: string }) => name)).toEqual(['迁移候选', '保留优先']);
  expect(await (await request.get(`${path}/runs`)).json()).toEqual([]);
  const savedPreview = await (await request.post(`${path}/preview`)).json();
  expect(savedPreview).toMatchObject({ revision: 1, candidateCount: 1, estimatedBytes: 100 });
  await page.reload();
  await page.getByRole('tab', { name: '策略配置' }).click();
  await expect(page.locator('pre')).toContainText('迁移候选');
  await page.getByRole('link', { name: '编辑策略' }).click();
  await page.getByLabel('描述').fill('浏览器尚未保存的修改');
  for (let i = 0; i < 5; i += 1) await next();

  current.definition.metadata.description = '另一个操作员的修改';
  const second = await request.put(path, {
    ...writer, headers: { ...writer.headers, 'If-Match': '"1"' }, data: current.definition,
  });
  expect(second.ok(), await second.text()).toBe(true);
  await saveWithWriter();
  await expect(page.getByText('修订冲突 · 当前修订 2')).toBeVisible();
  await expect(page.getByText('浏览器尚未保存的修改')).toBeVisible();
  await page.route(`**${path}`, (route) => route.fulfill({
    status: 503, json: { error: '暂时无法读取当前修订' },
  }), { times: 1 });
  page.once('dialog', (dialog) => dialog.accept());
  await page.getByRole('button', { name: '重新加载' }).click();
  await expect(page.getByText('暂时无法读取当前修订')).toBeVisible();
  await expect(page.getByText('浏览器尚未保存的修改')).toBeVisible();
  page.once('dialog', (dialog) => dialog.accept());
  await page.getByRole('button', { name: '重新加载' }).click();
  await expect(page.getByLabel('描述')).toHaveValue('另一个操作员的修改');

  const unsupported = structuredClone(current.definition);
  unsupported.metadata.name = `${name}-readonly`;
  unsupported.spec.execution.budget.run.maxObjectLogicalBytes = 'exact-budget';
  const raw = JSON.stringify(unsupported).replace('"exact-budget"', '9007199254740993');
  const created = await request.post('/api/policies', {
    ...writer, headers: { ...writer.headers, 'Content-Type': 'application/json' }, data: raw,
  });
  expect(created.ok(), await created.text()).toBe(true);
  const readOnly = await created.json();
  // This navigation deliberately leaves the reloaded, unmodified editor.
  await page.goto(`/console/#/policies/${readOnly.id}/revisions/1`);
  await expect(page.getByText(/此策略超出向导的可编辑范围/)).toBeVisible();
  await page.getByRole('tab', { name: '策略配置' }).click();
  await expect(page.locator('pre')).toContainText('9007199254740993');
  const downloadEvent = page.waitForEvent('download');
  await page.getByRole('button', { name: '导出策略 JSON' }).click();
  const download = await downloadEvent;
  expect(download.suggestedFilename()).toBe(`${name}-readonly-r1.json`);
  const stream = await download.createReadStream();
  const chunks = [];
  for await (const chunk of stream) chunks.push(chunk);
  expect(Buffer.concat(chunks).toString('utf-8')).toContain('9007199254740993');

  const active = structuredClone(current.definition);
  active.metadata.name = `${name}-enabled`;
  active.spec.enabled = true;
  active.spec.execution.activation.mode = 'preview';
  delete active.spec.target.directoryScope;
  const activeResponse = await request.post('/api/policies', { ...writer, data: active });
  expect(activeResponse.ok(), await activeResponse.text()).toBe(true);
  const { id: activeId } = await activeResponse.json();
  await page.goto(`/console/#/policies/${activeId}/edit`);
  await page.getByLabel('描述').fill('编辑后仍启用');
  for (let i = 0; i < 5; i += 1) await next();
  await saveWithWriter();
  await expect(page.getByRole('heading', { name: active.metadata.name, exact: true })).toBeVisible();
  const edited = await (await request.get(`/api/policies/${activeId}`)).json();
  expect(edited).toMatchObject({ revision: 2, definition: { spec: { enabled: true } } });
});
