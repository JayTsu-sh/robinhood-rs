import { Alert, Button, Card, Descriptions, Flex, Spin, Table, Typography } from 'antd';
import { useCallback, type ReactNode } from 'react';
import { ExecutionStatus, logicalBytes, operationStateOptions } from '@/components/ExecutionStatus';
import { DirectoryScopeResults } from '@/components/PolicyPreviewResult';
import {
  runReportUrl, summarizeOperations, type OperationFilters, type OperationResults, type RunDetail,
} from '@/services/executions';
import { useInventory } from '@/services/useInventory';

function OperationStateCounts({ counts, showEmpty = false }: { counts: Record<string, number>; showEmpty?: boolean }) {
  return (
    <Flex gap={12} wrap aria-label="操作状态分布">
      {operationStateOptions.filter(({ value }) => showEmpty || counts[value]).map(({ value }) => (
        <span key={value}><ExecutionStatus state={value} operation />{(counts[value] ?? 0).toLocaleString('zh-CN')}</span>
      ))}
    </Flex>
  );
}

export function OperationTotals({ results }: { results: OperationResults }) {
  return (
    <Descriptions column={{ xs: 1, md: 3 }} size="small" items={[
      { key: 'total', label: '操作记录数', children: results.matchedCount.toLocaleString('zh-CN') },
      { key: 'completed', label: '已完成文件逻辑字节', children: logicalBytes(results.completedLogicalBytes) },
      { key: 'pending', label: '待核对数', children: results.pendingCount.toLocaleString('zh-CN') },
    ]} />
  );
}

export function RunResults({ run }: { run: RunDetail }) {
  const results = run.operationResults;
  return (
    <Flex vertical gap={12}>
      {results && <Card size="small" title="全部文件操作">
        <OperationTotals results={results} />
        <OperationStateCounts counts={results.byState} showEmpty />
      </Card>}
      <RunDirectoryScope run={run} />
    </Flex>
  );
}

export function RunDirectoryScope({ run }: { run: RunDetail }) {
  return run.directoryScope ? (
    <Card size="small" title="目录范围初始评估">
      <DirectoryScopeResults scope={run.directoryScope} />
    </Card>
  ) : null;
}

export function ReportDownload({ id, filters }: { id: string; filters: OperationFilters }) {
  return (
    <Button href={runReportUrl(id, filters)} target="_blank" rel="noopener">
      {filters.exceptionsOnly ? '导出异常报告' : '导出运行报告'}
    </Button>
  );
}

export function FilteredOperationResults({
  id, filters, refreshKey, title, action, scope, showReasons,
}: {
  id: string;
  filters: OperationFilters;
  refreshKey: number;
  title: string;
  action?: ReactNode;
  scope?: string;
  showReasons?: boolean;
}) {
  const load = useCallback((signal: AbortSignal) => summarizeOperations(id, filters, signal), [id, filters]);
  const { state, refresh } = useInventory(load, load, { refreshKey });
  if (state.status === 'loading') return <Flex gap={8} role="status"><Spin size="small" />正在汇总操作</Flex>;
  if (state.status === 'error') {
    return <Alert type="error" showIcon message="读取操作汇总失败" description={state.message}
      action={<Button onClick={refresh}>重试</Button>} />;
  }
  const { data } = state;
  return (
    <Card size="small" title={title} extra={action}>
      {scope && <Typography.Paragraph type="secondary">{scope}</Typography.Paragraph>}
      <OperationTotals results={data} />
      <OperationStateCounts counts={data.byState} />
      {showReasons && <Table
        size="small"
        rowKey={({ reason }) => JSON.stringify(reason)}
        dataSource={data.byReason}
        pagination={false}
        locale={{ emptyText: '暂无符合筛选条件的异常原因' }}
        columns={[
          { title: '记录原因', dataIndex: 'reason', render: (reason) => (
            <Typography.Text className="console-mono console-execution-reason">{reason ?? '未知'}</Typography.Text>
          ) },
          { title: '状态分布', dataIndex: 'byState', render: (counts) => <OperationStateCounts counts={counts} /> },
          { title: '操作数', dataIndex: 'count', width: 110, render: (count) => count.toLocaleString('zh-CN') },
        ]}
      />}
    </Card>
  );
}
