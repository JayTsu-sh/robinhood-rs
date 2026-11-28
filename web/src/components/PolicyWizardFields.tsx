import { DeleteOutlined } from '@ant-design/icons';
import { Link } from '@umijs/max';
import { Alert, Button, Card, Checkbox, Col, Descriptions, Flex, Form, Input, Radio, Row, Select, Tag, Typography } from 'antd';
import {
  ageComparisons, builtinPreview, candidateOrders, comparisons, conditionFields, durationUnits, intervalUnits, isIdentityField,
  isNamespaceField, namespaceComparisons, newRule, sizeUnits,
  type Field, type FieldPath, type PolicySettings, type WizardStep,
} from '@/domain/tieringPolicy';
import type { WizardResources } from '@/services/policies';
import { Reason, formatBytes, formatObservationTime } from './ResourceStatus';

const options = (labels: Record<string, string>) =>
  Object.entries(labels).map(([value, label]) => ({ value, label }));
const selectValue = (value?: string) => ({ value: value || undefined });

function QuantityFields({ path, label, units }: { path: FieldPath; label: string; units: Record<string, string> }) {
  return (
    <Row gutter={8} align="top" wrap={false}>
      <Col flex="auto">
        <Form.Item name={[...path, 'amount']} label={label}>
          <Input inputMode="numeric" />
        </Form.Item>
      </Col>
      <Col flex="106px">
        <Form.Item name={[...path, 'unit']} label="单位">
          <Select options={options(units)} virtual={false} />
        </Form.Item>
      </Col>
    </Row>
  );
}

function ConditionFields({ path, title, empty, parent = [] }: {
  path: FieldPath; title: string; empty: string; parent?: FieldPath;
}) {
  const form = Form.useFormInstance();
  return (
    <Card size="small" title={title}>
      <Form.Item name={[...path, 'join']} label={`${title}组合`}>
        <Select style={{ maxWidth: 220 }} options={options({ all: 'AND · 全部满足', any: 'OR · 任一满足' })} virtual={false} />
      </Form.Item>
      <Form.List name={[...path, 'clauses']}>
        {(fields, { add, remove }) => (
          <>
            {fields.length === 0 && <Typography.Paragraph type="secondary">{empty}</Typography.Paragraph>}
            {fields.map(({ name, key }) => (
              <Form.Item key={key} noStyle shouldUpdate>
                {() => {
                  const clause = [...parent, ...path, 'clauses', name];
                  const field: Field = form.getFieldValue([...clause, 'field']);
                  const isSize = field === 'object.size';
                  const identity = isIdentityField(field);
                  const namespace = isNamespaceField(field);
                  return (
                    <Row gutter={[8, 0]} align="middle">
                      <Col xs={24} sm={8} md={7}>
                        <Form.Item name={[name, 'field']} label="文件属性">
                          <Select options={options(conditionFields)} virtual={false}
                            onChange={(value: Field) => {
                              const size = value === 'object.size';
                              const numeric = isIdentityField(value);
                              const text = isNamespaceField(value);
                              form.setFieldValue([...clause, 'op'], text ? 'glob' : numeric ? 'eq' : size ? 'gte' : 'older_than');
                              form.setFieldValue([...clause, 'unit'], text || numeric ? '' : size ? 'MiB' : 'd');
                              if (text) form.setFieldValue([...clause, 'amount'], '');
                            }} />
                        </Form.Item>
                      </Col>
                      <Col xs={12} sm={5} md={5}>
                        <Form.Item name={[name, 'op']} label="比较">
                          <Select options={options(namespace ? namespaceComparisons : isSize || identity ? comparisons : ageComparisons)} virtual={false} />
                        </Form.Item>
                      </Col>
                      <Col xs={12} sm={9} md={10}>
                        {namespace ? <Form.Item name={[name, 'amount']} label="路径或名称">
                          <Input className="console-mono" placeholder={field === 'object.path' ? '/team/tmp/*' : '*.lock'} />
                        </Form.Item> : identity ? <Form.Item name={[name, 'amount']} label="数值"><Input inputMode="numeric" /></Form.Item>
                          : <QuantityFields path={[name]} label="数值" units={isSize ? sizeUnits : durationUnits} />}
                      </Col>
                      <Col xs={24} sm={2} md={2}>
                        <Button type="text" danger icon={<DeleteOutlined />} aria-label={`删除条件 ${name + 1}`}
                          onClick={() => remove(name)} />
                      </Col>
                    </Row>
                  );
                }}
              </Form.Item>
            ))}
            <Button onClick={() => add({ field: 'object.size', op: 'gte', amount: '1', unit: 'MiB' })}>添加条件</Button>
          </>
        )}
      </Form.List>
    </Card>
  );
}

function BasicFields({ resources }: { resources: WizardResources }) {
  const form = Form.useFormInstance();
  const filesystemId = Form.useWatch('filesystem', { form, preserve: true });
  const source = Form.useWatch('sourcePool', { form, preserve: true });
  const eligible = resources.filesystems.filter(({ tieringAvailable }) => tieringAvailable);
  const filesystem = resources.filesystems.find(({ id }) => id === filesystemId);
  const pools = filesystem?.tieringAvailable ? filesystem.pools?.filter(({ eligible }) => eligible) ?? [] : [];
  const labels = pools.map(({ name, capacity }) => {
    const sample = capacity?.sample;
    return {
      value: name,
      label: name,
      sample: sample ? `可用 ${formatBytes(sample.availableBytes)} · ${formatObservationTime(sample.observedAt)}` : '容量未取得',
    };
  });
  const poolOption = ({ data }: { data: (typeof labels)[number] }) => (
    <Flex vertical>
      <span>{data.label}</span>
      <Typography.Text type="secondary">{data.sample}</Typography.Text>
    </Flex>
  );
  return (
    <>
      <Form.Item name="name" label="策略名称" required><Input maxLength={253} /></Form.Item>
      <Form.Item name="description" label="描述"><Input.TextArea autoSize={{ minRows: 2, maxRows: 5 }} /></Form.Item>
      <Form.Item label="策略模板">
        <Radio.Group value="pool" aria-label="策略模板" style={{ width: '100%' }}>
          <Row gutter={[12, 12]}>
            <Col xs={24} sm={12}>
              <Card size="small" style={{ height: '100%', borderColor: '#2563eb' }}>
                <Radio value="pool">集群内分层</Radio>
                <div><Typography.Text type="secondary">在同一 Lustre 文件系统的两个 OST 池之间迁移。</Typography.Text></div>
              </Card>
            </Col>
            <Col xs={24} sm={12}>
              <Card size="small" style={{ height: '100%' }}>
                <Radio value="hsm" disabled>跨存储分层（HSM）<Tag style={{ marginInlineStart: 8 }}>暂未开放</Tag></Radio>
                <div><Typography.Text type="secondary">将 Lustre 数据归档至已注册的外部存储。</Typography.Text></div>
              </Card>
            </Col>
          </Row>
        </Radio.Group>
      </Form.Item>
      {(!eligible.length || (filesystemId && !filesystem?.tieringAvailable)) && (
        <Alert type="warning" showIcon message={eligible.length ? '当前选择的文件系统不可用' : '暂无可用于分层的文件系统'}
          description={<>{filesystem ? <Reason reason={filesystem.reason} /> :
            resources.filesystems.map(({ id, reason }) => <div key={id}>{id}<Reason reason={reason} /></div>)}
            <Link to="/resources">查看 Lustre 资源</Link></>} />
      )}
      <Row gutter={16} style={{ marginTop: 12 }}>
        <Col xs={24} lg={8}>
          <Form.Item name="filesystem" label="Lustre 文件系统" required getValueProps={selectValue}>
            <Select allowClear placeholder="选择文件系统" options={eligible.map(({ id }) => ({ value: id, label: id }))} virtual={false} />
          </Form.Item>
        </Col>
        <Col xs={24} sm={12} lg={8}>
          <Form.Item name="sourcePool" label="源池" required getValueProps={selectValue}>
            <Select placeholder="选择源池" disabled={!pools.length} options={labels} optionRender={poolOption} virtual={false} />
          </Form.Item>
        </Col>
        <Col xs={24} sm={12} lg={8}>
          <Form.Item name="targetPool" label="目标池" required getValueProps={selectValue}>
            <Select placeholder="选择目标池" disabled={!source || !pools.length}
              options={labels.filter(({ value }) => value !== source)} optionRender={poolOption} virtual={false} />
          </Form.Item>
        </Col>
      </Row>
    </>
  );
}

function ScopeFields({ allowToggle }: { allowToggle: boolean }) {
  const form = Form.useFormInstance();
  const scoped = Form.useWatch('scoped', { form, preserve: true });
  return (
    <Flex vertical gap={12}>
      {allowToggle && (
        <Form.Item name="scoped" valuePropName="checked">
          <Checkbox>限定目录范围</Checkbox>
        </Form.Item>
      )}
      {scoped && (
        <Card size="small" title="目录范围">
          <Typography.Paragraph type="secondary">只处理普通文件，同一 FID 的所有硬链接都须满足范围。</Typography.Paragraph>
          <Form.Item name="root" label="起始目录" required><Input className="console-mono" placeholder="/team" /></Form.Item>
          <Form.Item name="recursive" valuePropName="checked" getValueProps={(value) => ({ checked: value !== false })}>
            <Checkbox>递归扫描子目录</Checkbox>
          </Form.Item>
          <Form.List name="excludedSubtrees">
            {(fields, { add, remove }) => (
              <>
                {fields.map(({ key, name }) => (
                  <Flex key={key} gap={8} align="center">
                    <Form.Item name={name} label={`排除子目录 ${name + 1}`} style={{ flex: 1 }}>
                      <Input className="console-mono" placeholder="/team/archive" />
                    </Form.Item>
                    <Button type="text" danger icon={<DeleteOutlined />} aria-label={`删除排除目录 ${name + 1}`}
                      onClick={() => remove(name)} />
                  </Flex>
                ))}
                <Button onClick={() => add('')}>添加排除目录</Button>
              </>
            )}
          </Form.List>
        </Card>
      )}
      <ConditionFields path={['include']} title="纳入条件" empty="空组包含范围内的所有普通文件。" />
      <ConditionFields path={['exclude']} title="排除条件" empty="空组无额外排除条件。排除条件优先。" />
    </Flex>
  );
}

function RuleFields() {
  return (
    <>
      <Typography.Paragraph type="secondary">从上往下取首条匹配。命中「保留」或无匹配时跳过，迁移动作使用同一池对。</Typography.Paragraph>
      <Form.List name="rules">
        {(fields, { add, move, remove }, { errors }) => (
          <Flex vertical gap={12}>
            {fields.map(({ name, key }) => (
              <Card key={key} size="small" title={`规则 ${name + 1}`}
                extra={<Flex gap={4} wrap>
                  <Button size="small" aria-label={`上移规则 ${name + 1}`} disabled={name === 0} onClick={() => move(name, name - 1)}>上移</Button>
                  <Button size="small" aria-label={`下移规则 ${name + 1}`} disabled={name + 1 === fields.length} onClick={() => move(name, name + 1)}>下移</Button>
                  <Button size="small" type="text" danger disabled={fields.length === 1}
                    aria-label={`删除规则 ${name + 1}`} onClick={() => remove(name)}>删除</Button>
                </Flex>}>
                <Row gutter={16}>
                  <Col xs={24} md={14}>
                    <Form.Item name={[name, 'name']} label={`规则名称 ${name + 1}`} required><Input /></Form.Item>
                  </Col>
                  <Col xs={24} md={10}>
                    <Form.Item name={[name, 'decision']} label={`动作 ${name + 1}`}>
                      <Select options={options({ execute: '池迁移', skip: '保留' })} virtual={false} />
                    </Form.Item>
                  </Col>
                </Row>
                <ConditionFields path={[name, 'condition']} parent={['rules']}
                  title="规则条件" empty="空组匹配所有剩余候选。" />
              </Card>
            ))}
            <Form.ErrorList errors={errors} />
            <div><Button onClick={() => add(newRule(fields.length + 1))}>添加规则</Button></div>
          </Flex>
        )}
      </Form.List>
    </>
  );
}

function TriggerFields() {
  const form = Form.useFormInstance();
  const trigger = Form.useWatch('trigger', { form, preserve: true });
  const firstTime = Form.useWatch('firstTime', { form, preserve: true });
  const windowEnabled = Form.useWatch('windowEnabled', { form, preserve: true });
  return (
    <>
      <Form.Item name="trigger" label="启动方式">
        <Select options={options({ periodic: '定期执行', pool_usage: '源 Pool 容量水位', manual: '仅手动触发' })} virtual={false} />
      </Form.Item>
      {trigger !== 'manual' && (
        <>
          <QuantityFields path={['interval']} label={trigger === 'pool_usage' ? '检测间隔' : '周期'} units={intervalUnits} />
          {trigger === 'pool_usage' ? <Row gutter={16}>
            <Col xs={24} sm={12}>
              <Form.Item name="highWatermark" label="高水位（%）· 启动"><Input inputMode="numeric" /></Form.Item>
            </Col>
            <Col xs={24} sm={12}>
              <Form.Item name="lowWatermark" label="低水位（%）· 停止"><Input inputMode="numeric" /></Form.Item>
            </Col>
          </Row> : <Row gutter={16}>
            <Col xs={24} sm={12}>
              <Form.Item name="firstTime" label="首次日内起点">
                <Input type="time" />
              </Form.Item>
            </Col>
            {firstTime && <Col xs={24} sm={12}>
              <Form.Item name="firstTimezone" label="首次启动时区" required><Input /></Form.Item>
            </Col>}
          </Row>}
        </>
      )}
      <Form.Item name="windowEnabled" valuePropName="checked">
        <Checkbox>限制每日准入窗口</Checkbox>
      </Form.Item>
      {windowEnabled && (
        <>
          <Typography.Paragraph type="secondary">窗口限制新动作准入。开始时刻晚于结束时刻时，窗口跨午夜。</Typography.Paragraph>
          <Row gutter={16}>
            <Col xs={24} md={12}>
              <Form.Item name="timezone" label="IANA 时区" required><Input placeholder="Asia/Shanghai" /></Form.Item>
            </Col>
            <Col xs={12} md={6}>
              <Form.Item name="start" label="开始时刻" required><Input type="time" /></Form.Item>
            </Col>
            <Col xs={12} md={6}>
              <Form.Item name="end" label="结束时刻" required><Input type="time" /></Form.Item>
            </Col>
          </Row>
        </>
      )}
    </>
  );
}

function ExecutionFields({ resources, settings, setMode }: {
  resources: WizardResources; settings: PolicySettings; setMode: (mode: PolicySettings['mode']) => void;
}) {
  const form = Form.useFormInstance();
  const selectedProfile = Form.useWatch('profile', { form, preserve: true });
  const selectedAuthorization = Form.useWatch('authorization', { form, preserve: true });
  const selectedApproval = Form.useWatch('approval', { form, preserve: true });
  const profile = resources.profile.find(({ name }) => name === selectedProfile)?.definition?.spec;
  const slots = (profile?.concurrency as { global?: number } | undefined)?.global;
  return (
    <>
      <Row gutter={16}>
        <Col xs={24} sm={12}><Form.Item name="maxOperations" label="单轮最大操作数"><Input inputMode="numeric" /></Form.Item></Col>
        <Col xs={24} sm={12}><Form.Item name="maxSubmitAttempts" label="单轮最大提交尝试数"><Input inputMode="numeric" /></Form.Item></Col>
        <Col xs={24} sm={12}><QuantityFields path={['maxLogicalBytes']} label="单轮最大逻辑数据量" units={sizeUnits} /></Col>
        <Col xs={24} sm={12}><QuantityFields path={['maxDuration']} label="单轮最大时长" units={durationUnits} /></Col>
        <Col xs={24} sm={12}><QuantityFields path={['maxFileBytes']} label="单文件逻辑大小上限" units={sizeUnits} /></Col>
        <Col xs={24} sm={12}><Form.Item name="maxRate" label="带宽上限（MiB/s）"><Input inputMode="numeric" placeholder="不限" /></Form.Item></Col>
        <Col xs={24} sm={12}><Form.Item name="maxConcurrency" label="同文件系统最大迁移并发"><Input inputMode="numeric" placeholder="1" /></Form.Item></Col>
        <Col xs={24} sm={12}><Form.Item name="minTargetFree" label="目标 Pool 最低空闲比例（%）"><Input inputMode="numeric" placeholder="不限" /></Form.Item></Col>
        <Col xs={24} sm={12}><Form.Item name="candidateOrder" label="候选排序">
          <Select allowClear placeholder="按 FID" options={options(candidateOrders)} virtual={false} />
        </Form.Item></Col>
        <Col xs={24} sm={12}><Form.Item name="retries" label="单对象重试次数"><Input inputMode="numeric" placeholder="0" /></Form.Item></Col>
        <Col xs={24} sm={12}><Form.Item name="pauseErrorRate" label="暂停错误率（%）"><Input inputMode="numeric" placeholder="留空时禁用" /></Form.Item></Col>
        <Col xs={24} sm={12}><Form.Item name="pauseMinFailures" label="暂停所需最少失败数"><Input inputMode="numeric" /></Form.Item></Col>
      </Row>
      <Card size="small" title="执行与治理资源">
        <Form.Item label="执行模式" htmlFor="execution-mode">
          <Select id="execution-mode" value={settings.mode} onChange={setMode}
            options={options({ preview: '仅评估（Dry-run）', enforced: '实际执行' })} virtual={false} />
        </Form.Item>
        {settings.mode === 'enforced' && <Form.Item name="requireOperatorApproval" valuePropName="checked">
          <Checkbox>正式执行前需要操作员批准</Checkbox>
        </Form.Item>}
        <Row gutter={16}>
          {([['profile', '执行配置'], ['authorization', '授权策略'], ['approval', '审批策略']] as const).map(([key, label]) => (
            <Col key={key} xs={24} lg={8}>
              <Form.Item name={key} label={label}>
                <Select options={resources[key].map(({ name, revision }) => ({ value: name, label: name, revision }))}
                  optionRender={({ data }) => `${data.label} · 修订 ${data.revision}`} virtual={false} />
              </Form.Item>
            </Col>
          ))}
        </Row>
        <Descriptions column={1} size="small" items={[
          { key: 'concurrency', label: '执行配置全局并发上限', children: slots ?? '—' },
          { key: 'parallelism', label: '并发准入', children: '实际并发受执行配置、未完成操作和 daemon 可用数据库连接限制。' },
          { key: 'constraints', label: '服务端准入', children: '执行配置、系统安全策略、HSM 资格和实时复核共同约束。实际绑定见预览和已保存修订。' },
          { key: 'files', label: '文件保护', children: '执行前复核文件标识、布局和状态。忙文件及证据不足的文件暂缓。' },
        ]} />
        {[selectedAuthorization, selectedApproval].includes(builtinPreview) && (
          <Alert type="info" showIcon message="builtin-preview 仅供预览。实际执行需绑定部署中允许池迁移的治理资源。" />
        )}
      </Card>
    </>
  );
}

export default function PolicyWizardFields({
  step, resources, settings, allowScopeToggle, setMode,
}: { step: WizardStep; resources: WizardResources; settings: PolicySettings; allowScopeToggle: boolean;
  setMode: (mode: PolicySettings['mode']) => void }) {
  switch (step) {
    case 'basics': return <BasicFields resources={resources} />;
    case 'filter': return <ScopeFields allowToggle={allowScopeToggle} />;
    case 'rules': return <RuleFields />;
    case 'trigger': return <TriggerFields />;
    case 'execution': return <ExecutionFields resources={resources} settings={settings} setMode={setMode} />;
    default: return null;
  }
}
