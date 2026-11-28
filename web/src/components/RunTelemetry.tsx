import { Card, Empty, Flex, Progress, Steps, Timeline, Typography } from 'antd';
import { useCallback } from 'react';
import { getRunTelemetry, type RunDetail } from '@/services/executions';
import { useInventory } from '@/services/useInventory';
import { executionTime, logicalBytes } from './ExecutionStatus';

const controlLabels: Record<string, string> = {
  pause: '操作员暂停', resume: '操作员恢复', cancel: '操作员取消', approve: '操作员批准',
};

export default function RunTelemetry({ run, refreshKey }: { run: RunDetail; refreshKey: number }) {
  const load = useCallback((signal: AbortSignal) => getRunTelemetry(run.idExact, signal), [run.idExact]);
  const { state } = useInventory(load, load, { keepPrevious: true, refreshKey });
  const telemetry = state.status === 'ready' ? state.data : undefined;
  const events = [
    { time: Date.parse(run.createdAt), label: '进入运行队列' },
    ...(run.startedAt ? [{ time: Date.parse(run.startedAt), label: '候选评估完成' }] : []),
    ...(run.finishedAt ? [{ time: Date.parse(run.finishedAt), label:
      run.state === 'previewed' ? '仅评估完成' : run.state === 'cancelled' ? '运行取消' :
      run.state === 'deferred' ? '运行暂缓' : '派发结束' }] : []),
    ...(telemetry?.controls.map(({ action, actor, occurredAtMillis }) =>
      ({ time: occurredAtMillis, label: `${controlLabels[action] ?? action}（${actor}）` })) ?? []),
  ].filter(({ time }) => Number.isFinite(time)).sort((a, b) => b.time - a.time);
  const count = run.operationResults?.matchedCount;
  const candidates = run.candidateCount;
  const progress = run.activationMode === 'enforced' && count !== undefined && candidates !== null && candidates > 0
    ? Math.min(100, Math.floor(100 * count / candidates)) : undefined;
  const samples = telemetry?.completions.slice().reverse().map(({ minute, logicalBytes: bytes }) => ({
    minute, bytes, mibPerSecond: Number(bytes) / (60 * 1024 * 1024),
  })) ?? [];
  const maxRate = Math.max(1, ...samples.map(({ mibPerSecond }) => mibPerSecond));
  const stage = run.state === 'queued' ? 0 : run.candidateCount == null || run.state === 'previewed' ? 1 :
    run.state === 'running' || run.state === 'awaiting_approval' || run.state === 'paused' ? 2 : 3;

  return (
    <>
      <Card size="small" title="候选处理进度">
        <Steps size="small" current={stage} items={[
          { title: '排队' }, { title: '候选评估' }, { title: '派发' }, { title: '核对与汇总' },
        ]} style={{ marginBottom: 12 }} />
        {progress !== undefined ? <>
          <Progress percent={progress} status="normal" />
          <Typography.Text type="secondary">
            已记录 {count?.toLocaleString('zh-CN')} 项文件操作，初始候选 {candidates?.toLocaleString('zh-CN')} 个。
          </Typography.Text>
        </> : <Typography.Text type="secondary">{run.startedAt ? '暂无可计算的操作进度' : '等待候选评估'}</Typography.Text>}
      </Card>
      <Card size="small" title="关键执行事件">
        <Timeline items={events.map(({ time, label }) => ({
          children: <><Typography.Text type="secondary">{executionTime(time)}</Typography.Text> · {label}</>,
        }))} />
      </Card>
      <Card size="small" title="完成吞吐趋势">
        {samples.length ? <>
          <Typography.Paragraph type="secondary">按每分钟确认完成的逻辑数据量折算为 MiB/s。</Typography.Paragraph>
          <Flex align="end" gap={4} style={{ height: 120 }} role="img" aria-label="每分钟确认完成的逻辑吞吐">
            {samples.map(({ minute, mibPerSecond, bytes }) => <div key={minute}
              title={`${executionTime(minute * 60_000)} · ${mibPerSecond.toFixed(2)} MiB/s · ${logicalBytes(bytes)}`}
              style={{ flex: 1, height: `${Math.max(2, 100 * mibPerSecond / maxRate)}%`, backgroundColor: '#2563eb' }}
            />)}
          </Flex>
          <Flex justify="space-between">
            <Typography.Text type="secondary">{executionTime(samples[0].minute * 60_000)}</Typography.Text>
            <Typography.Text type="secondary">{executionTime(samples[samples.length - 1].minute * 60_000)}</Typography.Text>
          </Flex>
        </> : <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="暂无已确认完成的吞吐样本" />}
      </Card>
    </>
  );
}
