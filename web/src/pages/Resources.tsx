import { ProTable, type ProColumns } from '@ant-design/pro-components';
import { Link, history } from '@umijs/max';
import { Button, Empty, Popover, Select, Space, Tag, Typography } from 'antd';
import { useState } from 'react';
import InventoryPage from '@/components/InventoryPage';
import {
  Availability, CapacityStatus, CollectionStatus, MountStatus, formatBytes,
  formatObservationTime, formatUsage, useObservationClock,
} from '@/components/ResourceStatus';
import { filesystemDetailsPath, listFilesystems, refreshFilesystems, type FilesystemResource } from '@/services/tiering';
import { useInventory } from '@/services/useInventory';

const resourceColumns = (now: number): ProColumns<FilesystemResource>[] => [
  {
    title: '文件系统 ID', dataIndex: 'id', width: 150,
    render: (_, { id }) => <Link to={filesystemDetailsPath(id)} className="console-mono">{id}</Link>,
  },
  {
    title: '挂载路径',
    dataIndex: 'mountPath',
    width: 185,
    renderText: (value) => <span className="console-mono">{value}</span>,
  },
  {
    title: '挂载状态',
    width: 106,
    render: (_, { mountStatus }) => <MountStatus status={mountStatus} />,
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
          <Typography.Text type="secondary">{eligible.length} 个</Typography.Text>
          {eligible.map(({ name }) => <Tag key={name} className="console-mono">{name}</Tag>)}
        </Space>
      ) : <Typography.Text type="secondary">尚无可选池</Typography.Text>;
    },
  },
  {
    title: '采集状态',
    width: 195,
    render: (_, { collectionStatus, observedAt, pools }) => {
      const failedPools = pools?.filter((pool) => pool.ostIndices === null) ?? [];
      return (
        <>
          <CollectionStatus status={collectionStatus} observedAt={observedAt} now={now} />
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
            <Typography.Text type="secondary">{formatObservationTime(observedAt)}</Typography.Text>
          </div>
        </>
      );
    },
  },
  {
    title: '总容量 / 使用率',
    width: 180,
    render: (_, { capacity }) => (
      <>
        <div>{formatBytes(capacity.sample?.totalBytes)} / {formatUsage(capacity.sample?.usedRatio)}</div>
        <CapacityStatus capacity={capacity} now={now} />
      </>
    ),
  },
  {
    title: '分层可用性',
    width: 215,
    render: (_, resource) => <Availability resource={resource} />,
  },
];

export default function Resources() {
  const { state, refresh } = useInventory(listFilesystems, refreshFilesystems);
  const now = useObservationClock();
  const [availability, setAvailability] = useState('all');
  const allRows = state.status === 'ready' ? state.data : [];
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
            virtual={false}
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
          columns={resourceColumns(now)}
          dataSource={rows}
          rowKey="id"
          onRow={({ id }) => ({
            onClick: ({ target, currentTarget }) => {
              if (target instanceof Element && currentTarget.contains(target) &&
                !target.closest('a, button') && window.getSelection()?.isCollapsed !== false) {
                history.push(filesystemDetailsPath(id));
              }
            },
            style: { cursor: 'pointer' },
          })}
          search={false}
          toolBarRender={false}
          pagination={rows.length > 10 ? { defaultPageSize: 10 } : false}
          scroll={{ x: 1250 }}
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
