import { ProTable, type ProColumns } from '@ant-design/pro-components';
import { useSearchParams } from '@umijs/max';
import { Button, Descriptions, Drawer, Empty, Flex, Input, Select, Typography } from 'antd';
import { Fragment, useCallback, useMemo, useState } from 'react';
import CursorResults from '@/components/CursorResults';
import { ExecutionStatus, executionTime, logicalBytes, operationStateOptions } from '@/components/ExecutionStatus';
import { FilteredOperationResults, ReportDownload } from '@/components/RunResults';
import {
  executionFilters, listOperations, operationFilters, operationStateFilterKey, type FileOperation, type OperationFilters,
} from '@/services/executions';

const troubleshooting: Record<string, string> = {
  source_pool_changed: '核对该 FID 的实际布局与策略源池。',
  source_generation_changed: '文件在运行期间发生变化。检查最近写入并重新计算候选。',
  indexed_link_paths_changed: '核对实际硬链接和索引中的路径，等待目录变更进入索引。',
  target_pool_free_space_below_minimum: '检查目标 Pool 空闲容量和在途迁移预留。',
  target_pool_capacity_unavailable: '检查目标 Pool 的 OST 容量采集是否完整。',
  unresolved_operation: '查看此 FID 仍待核对的执行记录。',
};

function OperationEvidence({ row, close }: { row: FileOperation | undefined; close: () => void }) {
  const evidence = row?.completionEvidence;
  const layout = evidence?.source === 'lustre_layout' ? evidence : undefined;
  const plan = row?.migrationPlan;
  const indices = layout?.ostIndices;
  const observedAt = layout?.observedAtMicros;
  return (
    <Drawer title="逐文件证据" open={Boolean(row)} onClose={close} width={560} style={{ maxWidth: '100vw' }}>
      {row && <>
        <Descriptions column={1} items={[
          { key: 'filesystem', label: '文件系统', children: row.filesystem },
          { key: 'fid', label: 'FID', children: <span className="console-mono">{row.fid}</span> },
          { key: 'paths', label: '派发时观测路径', children: row.paths?.join('、') || '—' },
          { key: 'state', label: '操作状态', children: <ExecutionStatus state={row.state} operation /> },
          { key: 'reason', label: '原因', children: <span className="console-mono console-execution-reason">{row.reason ?? '—'}</span> },
          ...(row.reason && troubleshooting[row.reason] ? [
            { key: 'troubleshooting', label: '排查建议', children: troubleshooting[row.reason] },
          ] : []),
          { key: 'action', label: '动作', children: row.action },
          { key: 'attempted', label: '曾尝试提交', children: row.attempted ? '是' : '否' },
          { key: 'retries', label: '重试次数', children: row.attemptCount == null ? '—' : Math.max(0, row.attemptCount - 1) },
          { key: 'size', label: '文件逻辑大小', children: logicalBytes(row.sizeExact) },
          { key: 'accepted', label: '受理时间', children: executionTime(row.acceptedAt) },
          { key: 'finished', label: '操作结束时间', children: executionTime(row.finishedAt) },
          { key: 'checked', label: '最后核对时间', children: executionTime(row.lastCheckedAt) },
          { key: 'source', label: '计划源池', children: plan?.sourcePool ?? '—' },
          { key: 'target', label: '计划目标池', children: plan?.targetPool ?? '—' },
          { key: 'stripe', label: '计划条带数 / 大小', children: plan
            ? `${plan.stripeCount} / ${logicalBytes(String(plan.stripeSize))}` : '—' },
          { key: 'pool', label: '实际目标池', children: typeof layout?.pool === 'string' ? layout.pool : '—' },
          { key: 'osts', label: '实际 OST 索引', children: Array.isArray(indices) && indices.length ? indices.join('、') : '—' },
          { key: 'observed', label: '布局观测时间', children: executionTime(typeof observedAt === 'number' ? observedAt / 1000 : null) },
        ]} />
        {evidence && !layout && (
          <pre className="console-mono" style={{ whiteSpace: 'pre-wrap' }}>{JSON.stringify(evidence, null, 2)}</pre>
        )}
      </>}
    </Drawer>
  );
}

const exceptionStateOptions = operationStateOptions.filter(({ value }) =>
  ['failed', 'deferred', 'unknown'].includes(value),
);

function OperationFilterForm({ filter, apply }: { filter: OperationFilters; apply: (filter: OperationFilters) => void }) {
  const [values, setValues] = useState(filter);
  const hasFilter = ({ state, reason, fid, path }: OperationFilters) => Boolean(state || reason || fid || path);
  return (
    <form className="console-controls" onSubmit={(event) => {
      event.preventDefault();
      apply(values);
    }}>
      <label>
        <Flex vertical gap={4}>文件 FID<Input placeholder="[0x200000401:0x1:0x0]"
          className="console-mono" maxLength={64} value={values.fid ?? ''}
          onChange={(event) => setValues({ ...values, fid: event.target.value })} /></Flex>
      </label>
      <label>
        <Flex vertical gap={4}>路径包含<Input value={values.path ?? ''} maxLength={4096}
          onChange={(event) => setValues({ ...values, path: event.target.value })} /></Flex>
      </label>
      <Flex vertical gap={4} className="console-execution-filter">
        <span>操作状态</span>
        <Select aria-label="操作状态" placeholder="全部操作状态" allowClear virtual={false}
          options={filter.exceptionsOnly ? exceptionStateOptions : operationStateOptions} value={values.state}
          onChange={(state) => setValues({ ...values, state })} />
      </Flex>
      <label>
        <Flex vertical gap={4}>原因包含<Input placeholder="输入已记录的原因" value={values.reason ?? ''} maxLength={255}
          onChange={(event) => setValues({ ...values, reason: event.target.value })} /></Flex>
      </label>
      <Flex gap={8} align="center" style={{ alignSelf: 'end' }}>
        <Button type="primary" htmlType="submit">查询</Button>
        <Button disabled={!hasFilter(filter) && !hasFilter(values)}
          onClick={() => { setValues({}); apply({}); }}>重置筛选</Button>
      </Flex>
    </form>
  );
}

export default function RunOperations({ id, refreshKey, exceptionsOnly }: { id: string; refreshKey: number; exceptionsOnly?: true }) {
  const [params, setParams] = useSearchParams();
  const [sequence, setSequence] = useState(0);
  const [selected, setSelected] = useState<string>();
  const filter = useMemo(() => operationFilters(params, exceptionsOnly), [params, exceptionsOnly]);
  const { fid, state, reason, path } = filter;
  const load = useCallback(
    (cursor: string | undefined, signal: AbortSignal) => listOperations(id, filter, cursor, signal),
    [id, filter],
  );
  const filterKey = JSON.stringify(filter);
  const columns: ProColumns<FileOperation>[] = [
    { title: 'FID / 文件系统', width: 245, render: (_, row) => (
      <Flex vertical gap={4}>
        <span className="console-mono">{row.fid}</span>
        <Typography.Text type="secondary">{row.filesystem}</Typography.Text>
      </Flex>
    ) },
    { title: '动作', dataIndex: 'action', width: 180 },
    { title: '操作状态', width: 112, render: (_, row) => <ExecutionStatus state={row.state} operation /> },
    { title: '派发时路径', width: 240, render: (_, { paths }) => paths?.length
      ? <span className="console-mono">{paths.join('、')}</span> : '—' },
    { title: '原因', width: 240, render: (_, row) => <span className="console-mono console-execution-reason">{row.reason ?? '—'}</span> },
    { title: '文件逻辑大小', width: 200, render: (_, row) => logicalBytes(row.sizeExact) },
    { title: '操作', width: 106, render: (_, row) => <Button type="link" onClick={() => setSelected(row.idExact)}>查看证据</Button> },
  ];

  function apply(values: OperationFilters) {
    setParams(executionFilters(params, {
      fid: values.fid, [operationStateFilterKey(exceptionsOnly)]: values.state, reason: values.reason, path: values.path,
    }));
    setSequence((value) => value + 1);
  }

  return (
    <div className="console-table">
      <OperationFilterForm key={filterKey} filter={filter} apply={apply} />
      <Fragment key={`${filterKey}/${sequence}`}>
        <FilteredOperationResults id={id} filters={filter} refreshKey={refreshKey}
          title={exceptionsOnly ? '异常原因汇总' : '操作汇总'} showReasons={exceptionsOnly}
          action={exceptionsOnly && <ReportDownload id={id} filters={filter} />} />
        <CursorResults load={load} refreshKey={refreshKey}>
          {(rows) => <>
            <ProTable columns={columns} rowKey="idExact" dataSource={rows} search={false}
              toolBarRender={false} pagination={false} scroll={{ x: 1323 }}
              locale={{ emptyText: <Empty className="console-empty" image={Empty.PRESENTED_IMAGE_SIMPLE}
                description={fid || state || reason || path ? '没有符合筛选条件的操作' :
                  exceptionsOnly ? '暂无异常操作' : '暂无逐文件操作'} /> }} />
            <OperationEvidence row={rows.find((row) => row.idExact === selected)} close={() => setSelected(undefined)} />
          </>}
        </CursorResults>
      </Fragment>
    </div>
  );
}
