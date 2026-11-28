import { Card, Descriptions, Empty, Flex, Typography } from 'antd';
import {
  comparisons, conditionFields, quantityText, steps, type ConditionGroup, type PolicySettings, type WizardValues,
} from '@/domain/tieringPolicy';
import type { PolicyRevision } from '@/services/policies';

function conditions(group: ConditionGroup, empty: string) {
  if (!group.clauses.length) return empty;
  return group.clauses.map((clause) => {
    const op = clause.op === 'older_than' ? '超过' : comparisons[clause.op as keyof typeof comparisons];
    return `${conditionFields[clause.field]} ${op} ${quantityText(clause)}`;
  }).join(group.join === 'all' ? ' AND ' : ' OR ');
}

export function PolicyConfiguration({ values, settings, confirmed = 5 }: {
  values: WizardValues; settings: PolicySettings; confirmed?: number;
}) {
  const { name, description, filesystem, sourcePool, targetPool } = values;
  const open = [
    [
      ['策略名称', name], ['描述', description || '—'], ['文件系统', filesystem],
      ['源池 → 目标池', `${sourcePool} → ${targetPool}`],
    ],
    [
      ['起始目录', values.scoped ? values.root : '未限定目录'],
      ['排除子目录', values.scoped ? values.excludedSubtrees.join('、') || '无' : '—'],
      ['纳入条件', conditions(values.include, '全部普通文件')],
      ['排除条件', conditions(values.exclude, '无额外条件')],
    ],
    values.rules.map(({ name, decision, condition }, i) => [
      `${i + 1}. ${name}`, `${decision === 'execute' ? '池迁移' : '保留'} · ${conditions(condition, '匹配所有剩余候选')}`,
    ]),
    [
      ['周期', quantityText(values.interval)],
      ['每日准入窗口', values.windowEnabled ?
        `${values.start}–${values.end} (${values.timezone}${values.start > values.end ? '，跨午夜' : ''})` : '不限'],
    ],
    [
      ['单轮操作数', values.maxOperations], ['单轮提交尝试数', values.maxSubmitAttempts],
      ['单轮逻辑数据量', quantityText(values.maxLogicalBytes)], ['单轮时长', quantityText(values.maxDuration)],
      ['单文件逻辑大小', quantityText(values.maxFileBytes)], ['执行配置', values.profile],
      ['授权策略', values.authorization], ['审批策略', values.approval],
      ['执行模式', settings.mode === 'enforced' ? '实际执行' : '仅预览'],
    ],
  ].slice(0, confirmed);

  if (!open.length) return <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="尚无已确认配置" />;
  return (
    <Flex vertical gap={12}>
      {open.map((section, index) => (
        <Card key={steps[index].id} size="small" title={steps[index].title}>
          <Descriptions size="small" column={1} items={section.map(([label, value]) => ({
            key: label, label, children: <span className="console-mono">{value}</span>,
          }))} />
        </Card>
      ))}
    </Flex>
  );
}

export function PolicyBindings({ bindings }: { bindings: PolicyRevision['resourceBindings'] }) {
  return (
    <Card size="small" title="资源修订">
      {bindings?.length ? (
        <Descriptions size="small" column={1} items={bindings.map(({ kind, name, revision }) => ({
          key: `${kind}/${name}`, label: kind, children: <span className="console-mono">{name} · 修订 {revision}</span>,
        }))} />
      ) : <Typography.Text type="secondary">此修订未记录资源绑定</Typography.Text>}
    </Card>
  );
}
