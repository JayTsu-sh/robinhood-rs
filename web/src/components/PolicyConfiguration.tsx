import { Card, Descriptions, Empty, Flex, Typography } from 'antd';
import {
  candidateOrders, comparisons, conditionFields, namespaceComparisons, quantityText, steps,
  type ConditionGroup, type PolicySettings, type WizardValues,
} from '@/domain/tieringPolicy';
import type { PolicyRevision } from '@/services/policies';

function conditions(group: ConditionGroup, empty: string) {
  if (!group.clauses.length) return empty;
  return group.clauses.map((clause) => {
    const op = clause.op === 'older_than' ? '超过' : clause.op === 'glob' ? namespaceComparisons.glob : comparisons[clause.op as keyof typeof comparisons];
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
      ['递归扫描', values.recursive === false ? '否' : '是'],
      ['排除子目录', values.scoped ? values.excludedSubtrees.join('、') || '无' : '—'],
      ['纳入条件', conditions(values.include, '全部普通文件')],
      ['排除条件', conditions(values.exclude, '无额外条件')],
    ],
    values.rules.map(({ name, decision, condition }, i) => [
      `${i + 1}. ${name}`, `${decision === 'execute' ? '池迁移' : '保留'} · ${conditions(condition, '匹配所有剩余候选')}`,
    ]),
    [
      ['启动方式', values.trigger === 'manual' ? '仅手动触发' : `每 ${quantityText(values.interval)}`],
      ...(values.trigger === 'pool_usage'
        ? [['源 Pool 高／低水位', `${values.highWatermark}% / ${values.lowWatermark}%`]] : []),
      ...((values.trigger ?? 'periodic') === 'periodic' && values.firstTime ?
        [['首次日内起点', `${values.firstTime} (${values.firstTimezone})`]] : []),
      ['每日准入窗口', values.windowEnabled ?
        `${values.start}–${values.end} (${values.timezone}${values.start > values.end ? '，跨午夜' : ''})` : '不限'],
    ],
    [
      ['单轮操作数', values.maxOperations], ['单轮提交尝试数', values.maxSubmitAttempts],
      ['单轮逻辑数据量', quantityText(values.maxLogicalBytes)], ['单轮时长', quantityText(values.maxDuration)],
      ['单文件逻辑大小', quantityText(values.maxFileBytes)], ['执行配置', values.profile],
      ...(values.maxRate ? [['带宽上限', `${values.maxRate} MiB/s`]] : []),
      ...(values.minTargetFree ? [['目标 Pool 最低空闲比例', `${values.minTargetFree}%`]] : []),
      ['候选排序', candidateOrders[values.candidateOrder ?? 'fid']],
      ['单对象重试次数', values.retries ?? '0'],
      ['同文件系统最大迁移并发', values.maxConcurrency ?? '1'],
      ...(values.pauseErrorRate ? [['错误率暂停', `${values.pauseErrorRate}% 且失败至少 ${values.pauseMinFailures} 次`]] : []),
      ['授权策略', values.authorization], ['审批策略', values.approval],
      ['执行模式', settings.mode === 'enforced' ? '实际执行' : '仅预览'],
      ...(settings.mode === 'enforced' ? [['逐次运行审批', values.requireOperatorApproval ? '需要' : '无需额外审批']] : []),
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
