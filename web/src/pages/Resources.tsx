import { ProTable, type ProColumns } from '@ant-design/pro-components';
import { Button, Empty, Popover, Select, Space, Tag, Typography } from 'antd';
import { useState } from 'react';
import InventoryPage from '@/components/InventoryPage';
import { listFilesystems, type FilesystemResource } from '@/services/tiering';
import { useInventory } from '@/services/useInventory';

const mountLabels = {
  mounted: ['success', '已挂载'],
  unmounted: ['default', '未挂载'],
  unknown: ['warning', '未知'],
} as const;

const collectionLabels = {
  collected: ['success', '已采集'],
  partial: ['warning', '部分采集'],
  unavailable: ['default', '不可采集'],
  error: ['error', '采集失败'],
} as const;

const resourceReasons: Record<string, string> = {
  not_a_lustre_mount: '配置路径未挂载 Lustre',
  read_only_mount: '挂载为只读',
  probe_failed: '采集任务失败',
  probe_timed_out: '采集超时，请刷新重试',
  lustre_hsm_capability_required: '当前 Rev.8 策略要求注册配置开启 Lustre HSM 能力',
  stripe_and_ost_capabilities_required: '注册配置需开启条带和 OST 能力',
  two_eligible_pools_required: '至少需要两个可用且不同的池',
};

const observationTime = new Intl.DateTimeFormat('zh-CN', {
  dateStyle: 'medium',
  timeStyle: 'medium',
});

const columns: ProColumns<FilesystemResource>[] = [
  { title: '文件系统 ID', dataIndex: 'id', width: 150 },
  {
    title: '挂载路径',
    dataIndex: 'mountPath',
    width: 185,
    renderText: (value) => <span className="console-mono">{value}</span>,
  },
  {
    title: '挂载状态',
    width: 106,
    render: (_, { mountStatus }) => {
      const [color, label] = mountLabels[mountStatus];
      return <Tag color={color}>{label}</Tag>;
    },
  },
  {
    title: '可选池',
    width: 220,
    render: (_, { pools }) => {
      if (!pools) return <Typography.Text type="secondary">无法确认可选池</Typography.Text>;
      const eligible = pools.filter((pool) => pool.eligible);
      if (!eligible.length && pools.some((pool) => pool.ostIndices === null)) {
        return <Typography.Text type="secondary">无法确认可选池</Typography.Text>;
      }
      return eligible.length ? (
        <Space wrap size={[4, 6]}>
          {eligible.map(({ name }) => <Tag key={name} className="console-mono">{name}</Tag>)}
        </Space>
      ) : <Typography.Text type="secondary">尚无可选池</Typography.Text>;
    },
  },
  {
    title: '采集状态',
    width: 195,
    render: (_, { collectionStatus, observedAt, pools }) => {
      const [color, label] = collectionLabels[collectionStatus];
      const failedPools = pools?.filter((pool) => pool.ostIndices === null) ?? [];
      return (
        <>
          <Tag color={color}>{label}</Tag>
          {failedPools.length > 0 && (
            <Popover
              title="池成员采集失败"
              trigger="click"
              content={<ul>{failedPools.map(({ name, reason }) => <li key={name}>{name} · {reason}</li>)}</ul>}
            >
              <Button type="link" size="small">查看原因</Button>
            </Popover>
          )}
          <div className="console-status-reason">
            <Typography.Text type="secondary">{observationTime.format(observedAt * 1000)}</Typography.Text>
          </div>
        </>
      );
    },
  },
  {
    title: '分层可用性',
    width: 215,
    render: (_, { tieringAvailable, reason }) => (
      <>
        <Tag color={tieringAvailable ? 'success' : 'default'}>{tieringAvailable ? '可用于分层' : '暂不可用于分层'}</Tag>
        {reason && (
          <div className="console-status-reason">
            <Typography.Text type="secondary">{resourceReasons[reason] ?? reason}</Typography.Text>
          </div>
        )}
      </>
    ),
  },
];

export default function Resources() {
  const { state, refresh } = useInventory(listFilesystems);
  const [availability, setAvailability] = useState('all');
  const allRows = state.status === 'ready' ? state.rows : [];
  const rows = allRows.filter(
    ({ tieringAvailable }) => availability === 'all' || tieringAvailable === (availability === 'available'),
  );

  return (
    <InventoryPage
      title="Lustre 集群"
      status={state.status}
      error={state.status === 'error' ? state.message : undefined}
      refresh={refresh}
    >
      <div className="console-table">
        <div className="console-controls">
          <Select
            aria-label="分层可用性"
            value={availability}
            onChange={setAvailability}
            options={[
              { value: 'all', label: '全部资源' },
              { value: 'available', label: '可用于分层' },
              { value: 'unavailable', label: '暂不可用于分层' },
            ]}
          />
        </div>
        <ProTable
          columns={columns}
          dataSource={rows}
          rowKey="id"
          search={false}
          toolBarRender={false}
          pagination={rows.length > 10 ? { defaultPageSize: 10 } : false}
          scroll={{ x: 1030 }}
          locale={{
            emptyText: <Empty className="console-empty" image={Empty.PRESENTED_IMAGE_SIMPLE} description={
              allRows.length ? '没有符合筛选条件的文件系统' : '当前 daemon 未注册 Lustre 文件系统'
            } />,
          }}
        />
      </div>
    </InventoryPage>
  );
}
