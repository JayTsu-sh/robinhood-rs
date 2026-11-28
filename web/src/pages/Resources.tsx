import { Link, history } from '@umijs/max';
import { Alert, Button, message, Popover, Select, Table, type TableColumnsType } from 'antd';
import { useEffect, useRef, useState } from 'react';
import InventoryPage from '@/components/InventoryPage';
import ResourceManagement, { useResourceManagement } from '@/components/ResourceManagement';
import {
  Availability, CapacityStatus, CollectionStatus, MountStatus, formatBytes,
  formatObservationTime, formatUsage, useObservationClock,
} from '@/components/ResourceStatus';
import { resourceOwnerLabels, type ResourceRegistration } from '@/services/resourceManagement';
import { filesystemDetailsPath, listFilesystems, refreshFilesystems, type FilesystemResource } from '@/services/tiering';
import { useInventory } from '@/services/useInventory';
import './resources.css';

function resourceColumns(
  now: number, registrations: Map<string, ResourceRegistration>,
): TableColumnsType<FilesystemResource> {
  return [
    {
      title: '集群名称 / 文件系统',
      width: 154,
      render: (_, { id, mount }) => {
        const registration = registrations.get(id);
        const name = registration?.name || id;
        const source = mount?.source || registration?.source;
        const filesystem = source?.match(/:\/([^/]+)/)?.[1] ?? id;
        const identifiers = [...new Set([filesystem, id])].filter((label) => label !== name);
        return (
          <>
            <Link to={filesystemDetailsPath(id)} className="lustre-table-link">{name}</Link>
            {identifiers.length > 0 && <div className="lustre-cell-note console-mono">{identifiers.join(' · ')}</div>}
          </>
        );
      },
    },
    {
      title: '挂载路径 / 所有权',
      width: 168,
      render: (_, { id, mountPath }) => {
        const registration = registrations.get(id);
        return (
          <>
            <span className="console-mono">{mountPath}</span>
            <div className="lustre-cell-note">{registration ? resourceOwnerLabels[registration.owner] : '—'}</div>
          </>
        );
      },
    },
    {
      title: '挂载状态',
      width: 92,
      render: (_, { id, mountStatus }) => {
        const error = registrations.get(id)?.lastMountError;
        return (
          <>
            <MountStatus status={mountStatus} />
            {mountStatus !== 'mounted' && error && (
              <div>
                <Popover title="最近挂载失败" trigger="click" content={<div className="lustre-mount-error">{error}</div>}>
                  <Button type="link" size="small" className="lustre-table-button">挂载异常</Button>
                </Popover>
              </div>
            )}
          </>
        );
      },
    },
    {
      title: '信息采集',
      width: 132,
      render: (_, { collectionStatus, observedAt, pools }) => {
        const failedPools = pools?.filter((pool) => pool.ostIndices === null) ?? [];
        return (
          <>
            <CollectionStatus status={collectionStatus} observedAt={observedAt} now={now} />
            {observedAt !== null && <div className="lustre-cell-note">{formatObservationTime(observedAt)}</div>}
            {failedPools.length > 0 && (
              <Popover
                title="池成员采集失败"
                trigger="click"
                content={<ul className="lustre-pool-list">
                  {failedPools.map(({ name, reason }) => <li key={name}>{name} · {reason}</li>)}
                </ul>}
              >
                <Button type="link" size="small" className="lustre-table-button">查看原因</Button>
              </Popover>
            )}
          </>
        );
      },
    },
    {
      title: 'Pool 数',
      width: 74,
      render: (_, { pools }) => {
        if (!pools?.length) return pools?.length ?? '—';
        return (
          <Popover
            title="已发现的 Pool"
            trigger="click"
            content={<ul className="lustre-pool-list">{pools.map(({ name, eligible }) => (
              <li key={name}>{name}{!eligible && <span className="lustre-cell-note"> · 不可用于分层</span>}</li>
            ))}</ul>}
          >
            <Button type="link" size="small" className="lustre-table-button" aria-label={`查看 ${pools.length} 个 Pool`}>
              {pools.length}
            </Button>
          </Popover>
        );
      },
    },
    {
      title: '总容量 / 使用率',
      width: 130,
      render: (_, { capacity }) => (
        <>
          <div>{formatBytes(capacity.sample?.totalBytes)}</div>
          {capacity.sample && <div className="lustre-cell-note">{formatUsage(capacity.sample.usedRatio)}</div>}
          <CapacityStatus capacity={capacity} now={now} hideFresh />
        </>
      ),
    },
    {
      title: '分层状态',
      width: 125,
      render: (_, resource) => <Availability resource={resource} compact />,
    },
    {
      title: '操作',
      width: 65,
      render: (_, { id }) => <Link to={filesystemDetailsPath(id)} className="lustre-table-link">详情</Link>,
    },
  ];
}

export default function Resources() {
  const { state, refresh } = useInventory(listFilesystems, refreshFilesystems);
  const { management, error: managementError, refresh: reloadManagement, refreshing: reloadingManagement } =
    useResourceManagement({ keepPrevious: true });
  const [notifications, notificationContext] = message.useMessage();
  const refreshRequested = useRef(false);
  const registrations = new Map(management?.resources.map((resource) => [resource.id, resource]));
  const now = useObservationClock();
  const [availability, setAvailability] = useState('all');
  useEffect(() => {
    if (!refreshRequested.current || state.status === 'loading' || reloadingManagement) return;
    refreshRequested.current = false;
    if (state.status === 'ready' && !managementError) void notifications.success('资源已刷新');
  }, [managementError, notifications, reloadingManagement, state.status]);

  const allRows = state.status === 'ready' ? state.data : [];
  const rows = allRows.filter(
    ({ tieringAvailable }) => availability === 'all' || tieringAvailable === (availability === 'available'),
  );
  const metrics = [
    { label: '已接入文件系统', value: allRows.length, caption: '本机 Lustre 客户端', color: 'blue' },
    {
      label: '已挂载', value: allRows.filter(({ mountStatus }) => mountStatus === 'mounted').length,
      caption: '包含应用与外部管理挂载', color: 'green',
    },
    {
      label: '分层不可用', value: allRows.filter(({ tieringAvailable }) => !tieringAvailable).length,
      caption: '挂载、采集或 Pool 异常', color: 'amber',
    },
    {
      label: '可用 Pool',
      value: allRows.reduce((sum, { tieringAvailable, pools }) =>
        sum + (tieringAvailable ? pools?.filter(({ eligible }) => eligible).length ?? 0 : 0), 0),
      caption: '按文件系统统计', color: 'green',
    },
  ];

  function refreshResources() {
    notifications.destroy();
    refresh();
    reloadManagement();
  }

  return (
    <div className="lustre-resources">
      {notificationContext}
      <InventoryPage
        title="Lustre 集群"
        description="管理本机客户端接入的 Lustre 文件系统、挂载及分层所需资源。"
        status={state.status}
        error={state.status === 'error' ? state.message : undefined}
        refresh={() => {
          refreshRequested.current = true;
          refreshResources();
        }}
        refreshLabel="刷新资源"
        refreshing={reloadingManagement}
        primaryAction={management && !managementError && !reloadingManagement && (
          <ResourceManagement management={management} onRefresh={refreshResources} />
        )}
      >
        {managementError && (
          <Alert type="warning" showIcon message="集群接入信息读取失败" description={managementError} />
        )}
        <dl className="lustre-metrics">
          {metrics.map(({ label, value, caption, color }) => (
            <div className={`lustre-metric lustre-metric-${color}`} key={label}>
              <dt>{label}</dt>
              <dd>{value}</dd>
              <dd className="lustre-metric-caption">{caption}</dd>
            </div>
          ))}
        </dl>
        <div className="console-table">
          <div className="console-controls lustre-toolbar">
            <Select
              aria-label="分层可用性"
              virtual={false}
              value={availability}
              onChange={setAvailability}
              options={[
                { value: 'all', label: '全部资源' },
                { value: 'available', label: '分层可用' },
                { value: 'unavailable', label: '分层不可用' },
              ]}
            />
            <span>本机 Lustre Client · 当前节点</span>
          </div>
          <Table
            columns={resourceColumns(now, registrations)}
            dataSource={rows}
            rowKey="id"
            onRow={({ id }) => ({
              onClick: ({ target, currentTarget }) => {
                if (target instanceof Element && currentTarget.contains(target) &&
                  !target.closest('a, button') && window.getSelection()?.isCollapsed !== false) {
                  history.push(filesystemDetailsPath(id));
                }
              },
              className: 'lustre-resource-row',
            })}
            pagination={false}
            scroll={{ x: 940 }}
            locale={{
              emptyText: <div className="lustre-empty">
                {allRows.length ? '没有符合筛选条件的文件系统' : '当前 daemon 未注册 Lustre 文件系统'}
              </div>,
            }}
          />
          <div className="lustre-table-footer">共 {rows.length} / {allRows.length} 个资源 · 以文件系统为接入单位</div>
        </div>
      </InventoryPage>
    </div>
  );
}
