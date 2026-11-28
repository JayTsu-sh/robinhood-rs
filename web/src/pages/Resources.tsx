import { Link, history } from '@umijs/max';
import { Alert, Button, Popover, Select, Table, type TableColumnsType } from 'antd';
import { useState } from 'react';
import InventoryPage from '@/components/InventoryPage';
import ResourceManagement, { useResourceManagement } from '@/components/ResourceManagement';
import {
  Availability, CapacityStatus, CollectionStatus, MountStatus, formatBytes,
  formatUsage, useObservationClock,
} from '@/components/ResourceStatus';
import { withLiveMountStatus, type ResourceRegistration } from '@/services/resourceManagement';
import { filesystemDetailsPath, listFilesystems, type FilesystemResource } from '@/services/tiering';
import { useInventory } from '@/services/useInventory';
import { useResourceEvents } from '@/services/useResourceEvents';
import './resources.css';

type ClusterRow = {
  key: string;
  mountPath: string;
  mountStatus: FilesystemResource['mountStatus'];
  source?: string;
  resource?: FilesystemResource;
  registration?: ResourceRegistration;
};

function resourceColumns(now: number): TableColumnsType<ClusterRow> {
  return [
    {
      title: '集群名称',
      width: 140,
      render: (_, { resource, registration }) => {
        if (!resource) return '—';
        const { id } = resource;
        return <Link to={filesystemDetailsPath(id)} className="lustre-table-link">{registration?.name || id}</Link>;
      },
    },
    {
      title: '文件系统',
      width: 125,
      render: (_, { resource, source }) => {
        const filesystem = source?.match(/:\/([^/]+)/)?.[1] ?? resource?.id;
        return (
          <>
            <span className="console-mono">{filesystem ?? '—'}</span>
            {!resource && source && <div className="lustre-cell-note lustre-mount-source console-mono">{source}</div>}
          </>
        );
      },
    },
    {
      title: '挂载路径',
      width: 148,
      render: (_, { mountPath }) => <span className="console-mono">{mountPath}</span>,
    },
    {
      title: '挂载状态',
      width: 92,
      render: (_, { registration, mountStatus }) => {
        const error = registration?.lastMountError;
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
      width: 116,
      render: (_, { resource }) => {
        if (!resource) return '—';
        const { collectionStatus, observedAt, pools } = resource;
        const failedPools = pools?.filter((pool) => pool.ostIndices === null) ?? [];
        return (
          <>
            <CollectionStatus status={collectionStatus} observedAt={observedAt} now={now} />
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
      render: (_, { resource }) => {
        const pools = resource?.pools;
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
      render: (_, { resource }) => {
        if (!resource) return '—';
        const { capacity } = resource;
        return <>
          <div>{formatBytes(capacity.sample?.totalBytes)}</div>
          {capacity.sample && <div className="lustre-cell-note">{formatUsage(capacity.sample.usedRatio)}</div>}
          <CapacityStatus capacity={capacity} now={now} hideFresh />
        </>;
      },
    },
    {
      title: '分层状态',
      width: 125,
      render: (_, { resource }) => resource ? <Availability resource={resource} compact /> : '—',
    },
    {
      title: '操作',
      width: 65,
      render: (_, { resource }) => resource && <Link to={filesystemDetailsPath(resource.id)} className="lustre-table-link">详情</Link>,
    },
  ];
}

export default function Resources() {
  const { state, refresh } = useInventory(listFilesystems, listFilesystems, { keepPrevious: true, queueRefresh: true });
  const { management, error: managementError, refresh: reloadManagement } = useResourceManagement();
  useResourceEvents(refresh, reloadManagement);
  const status = state.status === 'ready' && !management && !managementError ? 'loading' : state.status;
  const registrations = new Map(management?.resources.map((resource) => [resource.id, resource]));
  const now = useObservationClock();
  const [availability, setAvailability] = useState('all');

  const allRows: ClusterRow[] = (state.status === 'ready' ? state.data : []).map((snapshot) => {
    const registration = registrations.get(snapshot.id);
    const resource = withLiveMountStatus(snapshot, registration);
    return {
      key: resource.id, resource, registration, mountPath: resource.mountPath, mountStatus: resource.mountStatus,
      source: resource.mount?.source || registration?.source,
    };
  });
  const registeredPaths = new Set(allRows.map(({ mountPath }) => mountPath));
  for (const { path, source } of management?.availableMounts ?? []) {
    if (!registeredPaths.has(path)) allRows.push({ key: `mount:${path}`, mountPath: path, mountStatus: 'mounted', source });
  }
  const rows = allRows.filter(
    ({ resource }) => availability === 'all' || (resource?.tieringAvailable === true) === (availability === 'available'),
  );
  const mountedClusters = allRows.filter(({ mountStatus }) => mountStatus === 'mounted').length;
  const metrics = [
    { label: '正常集群', value: mountedClusters, color: 'green' },
    { label: '异常集群', value: allRows.length - mountedClusters, color: 'amber' },
    { label: '文件系统', value: allRows.length, color: 'blue' },
    {
      label: '存储池',
      value: allRows.reduce((sum, { resource }) =>
        sum + (resource?.tieringAvailable ? resource.pools?.filter(({ eligible }) => eligible).length ?? 0 : 0), 0),
      color: 'green',
    },
  ];

  function refreshResources() {
    refresh();
    reloadManagement();
  }

  return (
    <div className="lustre-resources">
      <InventoryPage
        title="Lustre 集群"
        description="管理已接入的 Lustre 集群、文件系统和存储池。"
        status={status}
        error={state.status === 'error' ? state.message : undefined}
        refreshError={state.status === 'ready' ? state.refreshError : undefined}
        retry={refreshResources}
        primaryAction={management?.canCreateMount && (
          <ResourceManagement onRefresh={refreshResources} />
        )}
      >
        {managementError && (
          <Alert type="warning" showIcon message="集群挂载信息读取失败" description={managementError}
            action={<Button onClick={reloadManagement}>重试</Button>} />
        )}
        <dl className="lustre-metrics">
          {metrics.map(({ label, value, color }) => (
            <div className={`lustre-metric lustre-metric-${color}`} key={label}>
              <dt>{label}</dt>
              <dd>{value}</dd>
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
          </div>
          <Table
            columns={resourceColumns(now)}
            dataSource={rows}
            rowKey="key"
            onRow={({ resource }) => resource ? ({
              onClick: ({ target, currentTarget }) => {
                if (target instanceof Element && currentTarget.contains(target) &&
                  !target.closest('a, button') && window.getSelection()?.isCollapsed !== false) {
                  history.push(filesystemDetailsPath(resource.id));
                }
              },
              className: 'lustre-resource-row',
            }) : {}}
            pagination={false}
            scroll={{ x: 1020 }}
            locale={{
              emptyText: <div className="lustre-empty">
                {allRows.length ? '没有符合筛选条件的文件系统' : '暂无 Lustre 资源'}
              </div>,
            }}
          />
          <div className="lustre-table-footer">共 {rows.length} / {allRows.length} 个资源（以文件系统为单位）</div>
        </div>
      </InventoryPage>
    </div>
  );
}
