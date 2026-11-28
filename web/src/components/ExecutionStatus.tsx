import { Link } from '@umijs/max';
import { Flex, Tag, Typography } from 'antd';
import { runDetailsPath, type LatestRun, type RunHistory } from '@/services/executions';

const runStates = {
  queued: { label: '排队中', color: 'default' },
  running: { label: '派发中', color: 'processing' },
  previewed: { label: '已评估', color: 'blue' },
  finished: { label: '派发已结束', color: 'blue' },
  deferred: { label: '已暂缓', color: 'warning' },
  cancelled: { label: '已取消', color: 'default' },
};

const operationStates = {
  submitting: { label: '提交中', color: 'processing' },
  submitted: { label: '已提交', color: 'processing' },
  unknown: { label: '结果未知', color: 'warning' },
  completed: { label: '已完成', color: 'success' },
  failed: { label: '失败', color: 'error' },
  deferred: { label: '暂缓', color: 'warning' },
};

const stateOptions = (states: Record<string, { label: string }>) =>
  Object.entries(states).map(([value, { label }]) => ({ value, label }));

export const runStateOptions = stateOptions(runStates);
export const operationStateOptions = stateOptions(operationStates);

export function ExecutionStatus({ state, operation = false }: { state: string; operation?: boolean }) {
  const states: Record<string, { label: string; color: string }> = operation ? operationStates : runStates;
  const { color, label } = states[state] ?? { color: 'default', label: state };
  return <Tag color={color}>{label}</Tag>;
}

const datetime = new Intl.DateTimeFormat('zh-CN', {
  year: 'numeric', month: '2-digit', day: '2-digit',
  hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false,
});

export function executionTime(time: string | number | null) {
  if (time === null) return '—';
  const date = new Date(time);
  return Number.isNaN(date.getTime()) ? '—' : datetime.format(date);
}

export function logicalBytes(value: string | null) {
  return value === null ? '—' : `${BigInt(value).toLocaleString('en-US')} B`;
}

export function executionMode(mode: string | null) {
  if (mode === 'enforced') return '正式执行';
  if (mode === 'preview') return '仅评估';
  return mode ?? '—';
}

export function executionTrigger({ requestedBy }: Pick<RunHistory, 'requestedBy'>) {
  return requestedBy === null ? '周期调度' : '手动提交';
}

export function LatestRunLink({ run }: { run?: LatestRun }) {
  return run ? (
    <Flex gap={6} vertical>
      <Flex gap={8} align="center" wrap>
        <Link to={runDetailsPath(run.id)}>#{run.id}</Link>
        <ExecutionStatus state={run.state} />
      </Flex>
      <Typography.Text type="secondary">开始 {executionTime(run.startedAt)}</Typography.Text>
    </Flex>
  ) : <Typography.Text type="secondary">未执行</Typography.Text>;
}
