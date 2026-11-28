import { ProTable, type ProColumns } from '@ant-design/pro-components';
import { Link, useSearchParams } from '@umijs/max';
import { Button, Empty, Flex, Input, Select, Typography } from 'antd';
import { useCallback, useMemo, useState } from 'react';
import CursorResults from '@/components/CursorResults';
import { ExecutionStatus, executionMode, executionTime, executionTrigger, logicalBytes, runStateOptions } from '@/components/ExecutionStatus';
import { executionFilters, listRuns, runDetailsPath, type RunFilters, type RunHistory } from '@/services/executions';
import { policyDetailsPath } from '@/services/policies';

const columns: ProColumns<RunHistory>[] = [
  { title: '运行 ID', width: 100, render: (_, row) => <Link to={runDetailsPath(row.idExact)}>#{row.idExact}</Link> },
  { title: '执行时的策略', width: 260, render: (_, row) => (
    <Link to={policyDetailsPath(row.policyIdExact, row.revisionExact)}>
      {row.policyName ?? `策略 #${row.policyIdExact}`} · 修订 {row.revisionExact}
    </Link>
  ) },
  { title: '触发 / 模式', width: 140, render: (_, row) => (
    <Flex vertical gap={4}>
      <span>{executionTrigger(row)}</span>
      <Typography.Text type="secondary">{executionMode(row.activationMode)}</Typography.Text>
    </Flex>
  ) },
  { title: '调度状态', width: 150, render: (_, row) => <ExecutionStatus state={row.state} /> },
  { title: '创建 / 开始时间', width: 200, render: (_, row) => (
    <Flex vertical gap={4}>
      <span>{executionTime(row.createdAt)}</span>
      <Typography.Text type="secondary">{executionTime(row.startedAt)}</Typography.Text>
    </Flex>
  ) },
  { title: '候选数 / 估算逻辑量', width: 215, render: (_, row) => (
    <Flex vertical gap={4}>
      <span>{row.candidateCount?.toLocaleString('zh-CN') ?? '—'}</span>
      <Typography.Text type="secondary">{logicalBytes(row.estimatedBytesExact)}</Typography.Text>
    </Flex>
  ) },
  { title: '完成 / 异常 / 待核对', width: 160, render: (_, { operationResults }) => {
    if (!operationResults) return '—';
    const { byState, pendingCount } = operationResults;
    const counts = [byState.completed ?? 0, (byState.failed ?? 0) + (byState.deferred ?? 0) + (byState.unknown ?? 0), pendingCount];
    return counts.map((count) => count.toLocaleString('zh-CN')).join(' / ');
  } },
];

function RunFilterForm({
  filter, policyId, apply,
}: {
  filter: RunFilters;
  policyId?: string;
  apply: (filter: RunFilters) => void;
}) {
  const [values, setValues] = useState(filter);
  const hasFilter = (input: RunFilters) => Boolean(input.runId || input.state || input.policyName ||
    (policyId === undefined && input.policyId));
  return (
    <form className="console-controls" onSubmit={(event) => {
      event.preventDefault();
      apply(values);
    }}>
      {!policyId && <>
        <label>
          <Flex vertical gap={4}>策略 ID<Input value={values.policyId ?? ''} inputMode="numeric"
            pattern="[1-9][0-9]*" maxLength={20} onChange={(event) => setValues({ ...values, policyId: event.target.value })} /></Flex>
        </label>
        <label>
          <Flex vertical gap={4}>执行时的策略名称<Input value={values.policyName ?? ''} maxLength={255}
            onChange={(event) => setValues({ ...values, policyName: event.target.value })} /></Flex>
        </label>
      </>}
      <label>
        <Flex vertical gap={4}>运行 ID<Input value={values.runId ?? ''} inputMode="numeric"
          pattern="[1-9][0-9]*" maxLength={20} onChange={(event) => setValues({ ...values, runId: event.target.value })} /></Flex>
      </label>
      <Flex vertical gap={4} className="console-execution-filter">
        <span>调度状态</span>
        <Select aria-label="调度状态" placeholder="全部调度状态" allowClear virtual={false}
          options={runStateOptions} value={values.state} onChange={(state) => setValues({ ...values, state })} />
      </Flex>
      <Flex gap={8} align="center" style={{ alignSelf: 'end' }}>
        <Button type="primary" htmlType="submit">查询</Button>
        <Button disabled={!hasFilter(filter) && !hasFilter(values)}
          onClick={() => { setValues({}); apply({}); }}>重置筛选</Button>
      </Flex>
    </form>
  );
}

export default function RunHistoryTable({ policyId }: { policyId?: string }) {
  const [params, setParams] = useSearchParams();
  const [sequence, setSequence] = useState(0);
  const policy = policyId ?? params.get('policyId');
  const policyName = policyId === undefined ? params.get('policyName') : null;
  const runId = params.get('runId');
  const state = params.get('runState');
  const filter = useMemo<RunFilters>(() => ({
    policyId: policy ?? undefined, policyName: policyName ?? undefined,
    runId: runId ?? undefined, state: state ?? undefined,
  }), [policy, policyName, runId, state]);
  const load = useCallback((cursor: string | undefined, signal: AbortSignal) => listRuns(filter, cursor, signal), [filter]);
  const filterKey = JSON.stringify(filter);

  function apply(values: RunFilters) {
    setParams(executionFilters(params, {
      policyId: policyId === undefined ? values.policyId : undefined,
      policyName: values.policyName, runId: values.runId, runState: values.state,
    }));
    setSequence((value) => value + 1);
  }

  const activeFilter = filter.policyName || filter.runId || filter.state || (filter.policyId && !policyId);
  return (
    <div className="console-table">
      <RunFilterForm key={filterKey} filter={filter} policyId={policyId} apply={apply} />
      <CursorResults key={`${filterKey}/${sequence}`} load={load} refreshable>
        {(rows) => <ProTable columns={columns} rowKey="idExact" dataSource={rows} search={false}
          toolBarRender={false} pagination={false} scroll={{ x: 1225 }}
          locale={{ emptyText: <Empty className="console-empty" image={Empty.PRESENTED_IMAGE_SIMPLE}
            description={activeFilter ? '没有符合筛选条件的运行' : '暂无执行记录'} /> }} />}
      </CursorResults>
    </div>
  );
}
