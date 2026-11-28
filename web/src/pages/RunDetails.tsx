import { Link, useParams, useSearchParams } from '@umijs/max';
import { Alert, Card, Checkbox, Descriptions, Flex, Tabs } from 'antd';
import { useCallback, useEffect, useMemo, useState } from 'react';
import { ExecutionStatus, executionElapsed, executionMode, executionTime, executionTrigger, isActiveRun, logicalBytes, operationStateOptions } from '@/components/ExecutionStatus';
import InventoryPage from '@/components/InventoryPage';
import PolicyJson from '@/components/PolicyJson';
import RunControls from '@/components/RunControls';
import RunOperations from '@/components/RunOperations';
import RunTelemetry from '@/components/RunTelemetry';
import { FilteredOperationResults, ReportDownload, RunDirectoryScope, RunResults } from '@/components/RunResults';
import { executionFilters, getRun, operationFilters, runHistoryPath, type RunDetail } from '@/services/executions';
import { policyDetailsPath } from '@/services/policies';
import { useInventory } from '@/services/useInventory';

function RunOverview({ run, refreshKey }: { run: RunDetail; refreshKey: number }) {
  const mode = run.policyMode === 'one_shot' ? '一次性' : run.policyMode === 'continuous' ? '持续运行' : run.policyMode;
  return (
    <Flex gap={12} vertical>
      <Card>
        <Descriptions column={{ xs: 1, md: 2 }} items={[
          { key: 'reason', label: '调度原因', children: <span className="console-mono">{run.reason ?? '—'}</span> },
          { key: 'trigger', label: '触发来源', children: executionTrigger(run) },
          { key: 'actor', label: '请求人', children: run.requestedBy ?? '—' },
          ...(run.approvedBy ? [{ key: 'approver', label: '本次批准人', children: run.approvedBy }] : []),
          { key: 'activation', label: '执行模式', children: executionMode(run.activationMode) },
          { key: 'mode', label: '策略模式', children: mode ?? '—' },
          { key: 'created', label: '创建时间', children: executionTime(run.createdAt) },
          { key: 'started', label: '开始时间', children: executionTime(run.startedAt) },
          { key: 'finished', label: '派发结束时间', children: executionTime(run.finishedAt) },
          { key: 'duration', label: '派发耗时', children: executionElapsed(run.startedAt, run.finishedAt) },
          { key: 'count', label: '候选数', children: run.candidateCount?.toLocaleString('zh-CN') ?? '—' },
          { key: 'bytes', label: '候选估算逻辑量', children: logicalBytes(run.estimatedBytesExact) },
        ]} />
      </Card>
      <RunResults run={run} />
      <RunTelemetry run={run} refreshKey={refreshKey} />
    </Flex>
  );
}

function RunReport({ run, params, refreshKey }: { run: RunDetail; params: URLSearchParams; refreshKey: number }) {
  const reports = useMemo(() => [operationFilters(params), operationFilters(params, true)], [params]);
  return (
    <Flex gap={12} vertical>
      {reports.map((filters) => {
        const { state, reason, fid, path, exceptionsOnly } = filters;
        const stateLabel = operationStateOptions.find(({ value }) => value === state)?.label ?? state;
        const scope = [
          stateLabel ?? (exceptionsOnly ? '失败、暂缓与结果未知' : '全部操作状态'),
          reason ? `原因包含「${reason}」` : undefined,
          fid ? `FID ${fid}` : undefined,
          path ? `路径包含「${path}」` : undefined,
        ].filter(Boolean).join(' · ');
        return <FilteredOperationResults key={JSON.stringify(filters)}
          id={run.idExact} filters={filters} refreshKey={refreshKey} scope={scope}
          title={exceptionsOnly ? '异常报告' : '运行报告'}
          action={<ReportDownload id={run.idExact} filters={filters} />} />;
      })}
      <RunDirectoryScope run={run} />
      <PolicyJson id={run.policyIdExact} revision={run.revisionExact} />
    </Flex>
  );
}

function RunDetailsPage({ id }: { id: string }) {
  const [params, setParams] = useSearchParams();
  const [refreshKey, setRefreshKey] = useState(0);
  const [autoRefresh, setAutoRefresh] = useState(true);
  const selectedTab = params.get('tab');
  const tab = selectedTab === 'operations' || selectedTab === 'exceptions' || selectedTab === 'report' ? selectedTab : 'overview';
  const load = useCallback((signal: AbortSignal) => getRun(id, signal), [id]);
  const { state, refresh, refreshing } = useInventory(load, load, { keepPrevious: true });
  const run = state.status === 'ready' ? state.data : undefined;
  const refreshRun = useCallback(() => {
    refresh();
    setRefreshKey((value) => value + 1);
  }, [refresh]);
  const updating = run && (isActiveRun(run.state) || run.hasPendingOperations);
  useEffect(() => {
    if (!updating || !autoRefresh || refreshing) return;
    const timer = window.setTimeout(() => {
      if (document.visibilityState === 'visible') refreshRun();
      else {
        // Refresh immediately when the operator returns to this tab.
        document.addEventListener('visibilitychange', refreshRun, { once: true });
      }
    }, 5000);
    return () => {
      window.clearTimeout(timer);
      document.removeEventListener('visibilitychange', refreshRun);
    };
  }, [autoRefresh, updating, refreshing, refreshRun]);

  return (
    <InventoryPage title={`运行 #${id}`} status={state.status}
      error={state.status === 'error' ? state.message : undefined} refreshing={refreshing}
      refresh={refreshRun}
      action={<>
        <Link to="/runs">返回执行记录</Link>
        {run && <RunControls run={run} refresh={refreshRun} />}
      </>}>
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
          {updating && <Checkbox checked={autoRefresh} onChange={(event) => setAutoRefresh(event.target.checked)}>自动刷新</Checkbox>}
        </Flex>
        {run.hasPendingOperations && <Alert type="warning" showIcon message="仍有文件操作待核对"
          description="刷新可读取后端最新的核对结果。" />}
        <Tabs activeKey={tab} destroyOnHidden onChange={(value) => {
          setParams(executionFilters(params, { tab: value === 'overview' ? undefined : value }));
        }} items={[
          { key: 'overview', label: '执行总览', children: <RunOverview run={run} refreshKey={refreshKey} /> },
          { key: 'operations', label: '逐文件明细', children: <RunOperations id={id} refreshKey={refreshKey} /> },
          { key: 'exceptions', label: '异常明细', children: <Flex vertical gap={12}>
            <RunDirectoryScope run={run} />
            <RunOperations id={id} refreshKey={refreshKey} exceptionsOnly />
          </Flex> },
          { key: 'report', label: '报告', children: <RunReport run={run} params={params} refreshKey={refreshKey} /> },
        ]} />
      </Flex>}
    </InventoryPage>
  );
}

export default function RunDetails() {
  const { id = '' } = useParams();
  return <RunDetailsPage key={id} id={id} />;
}
