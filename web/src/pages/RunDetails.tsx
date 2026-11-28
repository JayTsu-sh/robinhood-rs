import { Link, useParams, useSearchParams } from '@umijs/max';
import { Alert, Card, Descriptions, Flex, Tabs } from 'antd';
import { useCallback, useState } from 'react';
import { ExecutionStatus, executionMode, executionTime, executionTrigger, logicalBytes } from '@/components/ExecutionStatus';
import InventoryPage from '@/components/InventoryPage';
import RunOperations from '@/components/RunOperations';
import { executionFilters, getRun, runHistoryPath, type RunDetail } from '@/services/executions';
import { policyDetailsPath } from '@/services/policies';
import { useInventory } from '@/services/useInventory';

function RunOverview({ run }: { run: RunDetail }) {
  const mode = run.policyMode === 'one_shot' ? '一次性' : run.policyMode === 'continuous' ? '持续运行' : run.policyMode;
  return (
    <Card>
      <Descriptions column={{ xs: 1, md: 2 }} items={[
        { key: 'reason', label: '调度原因', children: <span className="console-mono">{run.reason ?? '—'}</span> },
        { key: 'trigger', label: '触发来源', children: executionTrigger(run) },
        { key: 'actor', label: '请求人', children: run.requestedBy ?? '—' },
        { key: 'activation', label: '执行模式', children: executionMode(run.activationMode) },
        { key: 'mode', label: '策略模式', children: mode ?? '—' },
        { key: 'created', label: '创建时间', children: executionTime(run.createdAt) },
        { key: 'started', label: '开始时间', children: executionTime(run.startedAt) },
        { key: 'count', label: '候选数', children: run.candidateCount?.toLocaleString('zh-CN') ?? '—' },
        { key: 'bytes', label: '估算逻辑量', children: logicalBytes(run.estimatedBytesExact) },
      ]} />
    </Card>
  );
}

function RunDetailsPage({ id }: { id: string }) {
  const [params, setParams] = useSearchParams();
  const [refreshKey, setRefreshKey] = useState(0);
  const selectedTab = params.get('tab');
  const tab = selectedTab === 'operations' || selectedTab === 'exceptions' ? selectedTab : 'overview';
  const load = useCallback((signal: AbortSignal) => getRun(id, signal), [id]);
  const { state, refresh, refreshing } = useInventory(load, load, { keepPrevious: true });
  const run = state.status === 'ready' ? state.data : undefined;

  return (
    <InventoryPage title={`运行 #${id}`} status={state.status}
      error={state.status === 'error' ? state.message : undefined} refreshing={refreshing}
      refresh={() => { refresh(); setRefreshKey((value) => value + 1); }}
      action={<Link to="/runs">返回执行记录</Link>}>
      {run && <Flex gap={12} vertical>
        {state.status === 'ready' && state.refreshError && <Alert showIcon type="warning"
          message="刷新运行状态失败" description={state.refreshError} />}
        <Flex gap={16} align="center" wrap>
          <Flex gap={8} align="center">
            <span>调度状态</span>
            <ExecutionStatus state={run.state} />
          </Flex>
          <Link to={policyDetailsPath(run.policyIdExact, run.revisionExact)}>
            {run.policyName ?? `策略 #${run.policyIdExact}`} · 修订 {run.revisionExact}
          </Link>
          <Link to={runHistoryPath(run.policyIdExact)}>该策略的执行记录</Link>
        </Flex>
        {run.hasPendingOperations && <Alert type="warning" showIcon message="仍有文件操作待核对"
          description="刷新可读取后端最新的核对结果。" />}
        <Tabs activeKey={tab} destroyOnHidden onChange={(value) => {
          setParams(executionFilters(params, { tab: value === 'overview' ? undefined : value }));
        }} items={[
          { key: 'overview', label: '执行总览', children: <RunOverview run={run} /> },
          { key: 'operations', label: '逐文件明细', children: <RunOperations id={id} refreshKey={refreshKey} /> },
          { key: 'exceptions', label: '异常明细', children: <RunOperations id={id} refreshKey={refreshKey} exceptionsOnly /> },
        ]} />
      </Flex>}
    </InventoryPage>
  );
}

export default function RunDetails() {
  const { id = '' } = useParams();
  return <RunDetailsPage key={id} id={id} />;
}
