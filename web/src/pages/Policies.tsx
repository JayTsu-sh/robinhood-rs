import { SearchOutlined } from '@ant-design/icons';
import { ProTable, type ProColumns } from '@ant-design/pro-components';
import { Link, useSearchParams } from '@umijs/max';
import { Button, Empty, Flex, Input, Select, Tag } from 'antd';
import { useState } from 'react';
import { LatestRunLink, runStateOptions } from '@/components/ExecutionStatus';
import InventoryPage from '@/components/InventoryPage';
import PolicyExecutionActions from '@/components/PolicyExecutionActions';
import { policyDetailsPath, policyEditPath } from '@/services/policies';
import { filesystemDetailsPath, listTieringPolicies, type TieringPolicy } from '@/services/tiering';
import { useInventory } from '@/services/useInventory';

const columns: ProColumns<TieringPolicy>[] = [
  {
    title: '策略名称',
    dataIndex: 'name',
    width: 220,
    render: (_, row) => <Link to={policyDetailsPath(row.id)}>{row.name}</Link>,
  },
  { title: '策略 ID', dataIndex: 'id', width: 105 },
  { title: '当前修订', dataIndex: 'revision', width: 110 },
  {
    title: '文件系统',
    dataIndex: 'filesystem',
    width: 145,
    render: (_, { filesystem }) => <Link className="console-mono" to={filesystemDetailsPath(filesystem)}>{filesystem}</Link>,
  },
  {
    title: '源池 → 目标池',
    width: 210,
    render: (_, row) => <span className="console-mono">{row.sourcePool} → {row.targetPool}</span>,
  },
  {
    title: '启用状态',
    width: 120,
    render: (_, row) => <Tag color={row.enabled ? 'success' : 'default'}>{row.enabled ? '已启用' : '已停用'}</Tag>,
  },
  {
    title: '当前或最近运行',
    width: 205,
    render: (_, row) => <LatestRunLink run={row.latestRun} />,
  },
];

const loadPolicies = (signal: AbortSignal) => listTieringPolicies(signal, true);

export default function Policies() {
  const { state, refresh } = useInventory(loadPolicies);
  const actions: ProColumns<TieringPolicy> = {
    title: '操作',
    width: 175,
    render: (_, row) => <Flex gap={12} align="center" wrap>
      {row.editable && <Link to={policyEditPath(row.id)}>编辑</Link>}
      <PolicyExecutionActions key={row.revision} {...row} onRefresh={refresh} />
    </Flex>,
  };
  const [search, setSearch] = useState('');
  const [enabled, setEnabled] = useState('all');
  const [runState, setRunState] = useState('all');
  const [params, setParams] = useSearchParams();
  const policyId = params.get('policy');
  const allRows = state.status === 'ready' ? state.data : [];
  const query = search.trim().toLowerCase();
  const rows = allRows.filter((row) =>
    (!policyId || String(row.id) === policyId) &&
    (enabled === 'all' || row.enabled === (enabled === 'enabled')) &&
    (runState === 'all' || (row.latestRun?.state ?? 'never') === runState) &&
    [row.name, String(row.id), row.filesystem, row.sourcePool, row.targetPool]
      .some((value) => value.toLowerCase().includes(query)),
  );

  return (
    <InventoryPage
      title="分层策略"
      status={state.status}
      error={state.status === 'error' ? state.message : undefined}
      refresh={refresh}
      action={<Flex gap={12} align="center">
        <Link to="/policies/preview">预览迁移</Link>
        <Link to="/policies/new" className="ant-btn ant-btn-primary">新建策略</Link>
      </Flex>}
    >
      <div className="console-table">
        <div className="console-controls">
          <Input
            className="console-search"
            aria-label="搜索策略"
            type="search"
            prefix={<SearchOutlined />}
            placeholder="搜索名称、ID、文件系统或池"
            value={search}
            onChange={(event) => setSearch(event.target.value)}
          />
          <Select
            aria-label="策略启用状态"
            value={enabled}
            onChange={setEnabled}
            options={[
              { value: 'all', label: '全部状态' },
              { value: 'enabled', label: '已启用' },
              { value: 'disabled', label: '已停用' },
            ]}
          />
          <Select aria-label="当前或最近运行状态" value={runState} onChange={setRunState} virtual={false} options={[
            { value: 'all', label: '全部运行状态' },
            { value: 'never', label: '未执行' },
            ...runStateOptions,
          ]} />
          <Button
            disabled={!policyId && !search && enabled === 'all' && runState === 'all'}
            onClick={() => {
              setSearch('');
              setEnabled('all');
              setRunState('all');
              if (policyId) {
                const next = new URLSearchParams(params);
                next.delete('policy');
                setParams(next);
              }
            }}
          >
            重置筛选
          </Button>
          {policyId && <Tag>策略 ID {policyId}</Tag>}
        </div>
        <ProTable
          columns={[...columns, actions]}
          dataSource={rows}
          rowKey="id"
          search={false}
          toolBarRender={false}
          pagination={rows.length > 10 ? { defaultPageSize: 10 } : false}
          scroll={{ x: 1130 }}
          locale={{
            emptyText: <Empty className="console-empty" image={Empty.PRESENTED_IMAGE_SIMPLE} description={
              allRows.length ? '没有符合筛选条件的策略' : '暂无分层策略'
            } />,
          }}
        />
      </div>
    </InventoryPage>
  );
}
