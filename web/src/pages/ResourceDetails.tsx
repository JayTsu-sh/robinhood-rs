import { ArrowLeftOutlined } from '@ant-design/icons';
import { ProTable, type ProColumns } from '@ant-design/pro-components';
import { Link, useSearchParams } from '@umijs/max';
import { Alert, Button, Descriptions, Empty, Result, Space, Spin, Tabs, Tag, Typography } from 'antd';
import { useCallback } from 'react';
import InventoryPage from '@/components/InventoryPage';
import ResourceManagement, { useResourceManagement } from '@/components/ResourceManagement';
import {
  Availability, CapacityStatus, CollectionStatus, MountStatus, Reason, formatBytes,
  formatObservationTime, formatUsage, useObservationClock,
} from '@/components/ResourceStatus';
import { getFilesystem, listTieringPolicies, type FilesystemResource, type PoolResource } from '@/services/tiering';
import { useInventory } from '@/services/useInventory';
import { useResourceEvents } from '@/services/useResourceEvents';
import { poolTiers, resourceOwnerLabels, withLiveMountStatus, type ResourceRegistration } from '@/services/resourceManagement';
import './resource-details.css';

function RelatedPolicies({ filesystem }: { filesystem: string }) {
  const { state, refresh } = useInventory(listTieringPolicies);
  if (state.status === 'loading') return <Space role="status"><Spin size="small" />正在读取关联策略</Space>;
  if (state.status === 'error') {
    return <Alert type="error" showIcon message="读取关联策略失败" description={state.message}
      action={<Button onClick={refresh}>重试</Button>} />;
  }
  const related = state.data.filter((policy) => policy.filesystem === filesystem);
  if (!related.length) return <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="暂无关联分层策略" />;
  return (
    <ul className="resource-policy-list">
      {related.map(({ id, name, sourcePool, targetPool, enabled }) => (
        <li key={id}>
          <Space wrap>
            <Link to={`/policies?policy=${id}`}>{name}</Link>
            <Typography.Text type="secondary" className="console-mono">{sourcePool} → {targetPool}</Typography.Text>
          </Space>
          <Tag color={enabled ? 'success' : 'default'}>{enabled ? '已启用' : '已停用'}</Tag>
        </li>
      ))}
    </ul>
  );
}

function Overview({ resource, now }: { resource: FilesystemResource; now: number }) {
  const { mountPath, mountStatus, collectionStatus, observedAt, capacity } = resource;
  return (
    <>
      <Descriptions column={{ xs: 1, sm: 2, lg: 3 }} items={[
        { key: 'path', label: '挂载路径', children: <span className="console-mono">{mountPath}</span> },
        { key: 'mount', label: '挂载状态', children: <MountStatus status={mountStatus} /> },
        { key: 'collection', label: '资源采集', children: <Space wrap><CollectionStatus status={collectionStatus} observedAt={observedAt} now={now} />{formatObservationTime(observedAt)}</Space> },
        { key: 'tiering', label: '分层可用性', children: <div><Availability resource={resource} /></div> },
        { key: 'capacity', label: '容量采集', children: <div><CapacityStatus capacity={capacity} now={now} /></div> },
      ]} />
      <dl className="resource-capacity">
        {[
          ['总容量', formatBytes(capacity.sample?.totalBytes)],
          ['已用容量', formatBytes(capacity.sample?.usedBytes)],
          ['可用容量', formatBytes(capacity.sample?.availableBytes)],
          ['使用率', formatUsage(capacity.sample?.usedRatio)],
        ].map(([label, value]) => (
          <div key={label}><dt>{label}</dt><dd>{value}</dd></div>
        ))}
      </dl>
      <Typography.Title level={2} className="resource-subheading">关联分层策略</Typography.Title>
      <RelatedPolicies filesystem={resource.id} />
    </>
  );
}

function PoolMembers({ indices }: { indices: number[] | null }) {
  if (!indices) return <Typography.Text type="secondary">未取得成员</Typography.Text>;
  if (!indices.length) return <Typography.Text type="secondary">0 个</Typography.Text>;
  const names = indices.map((index) => `OST${index.toString(16).padStart(4, '0')}`).join('、');
  if (indices.length > 8) {
    return <details><summary>{indices.length} 个 OST</summary><div className="console-mono">{names}</div></details>;
  }
  return <>{indices.length} 个 · <span className="console-mono">{names}</span></>;
}

function Storage({ resource, now, registration }: { resource: FilesystemResource; now: number; registration?: ResourceRegistration }) {
  const { pools } = resource;
  if (pools === null) {
    return <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={<><div>未取得 OST 池信息</div><Reason reason={resource.reason} /></>} />;
  }
  const columns: ProColumns<PoolResource>[] = [
    { title: '池名称', dataIndex: 'name', width: 140, renderText: (value) => <span className="console-mono">{value}</span> },
    ...(registration ? [{
      title: '应用侧层级', width: 110,
      render: (_: unknown, { name }: PoolResource) => poolTiers[registration.tiers[name]] ?? '未分类',
    }] : []),
    { title: 'OST 成员', width: 235, render: (_, { ostIndices }) => <PoolMembers indices={ostIndices} /> },
    { title: '总容量', width: 100, render: (_, { capacity }) => formatBytes(capacity.sample?.totalBytes) },
    { title: '已用容量', width: 100, render: (_, { capacity }) => formatBytes(capacity.sample?.usedBytes) },
    { title: '可用容量', width: 100, render: (_, { capacity }) => formatBytes(capacity.sample?.availableBytes) },
    { title: '使用率', width: 85, render: (_, { capacity }) => formatUsage(capacity.sample?.usedRatio) },
    { title: '容量采集', width: 195, render: (_, { capacity }) => <CapacityStatus capacity={capacity} now={now} /> },
  ];
  return (
    <>
      <ProTable columns={columns} dataSource={pools} rowKey="name" search={false} toolBarRender={false}
        scroll={{ x: 955 }} pagination={pools.length > 10 ? { defaultPageSize: 10 } : false}
        locale={{ emptyText: <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description="尚未发现 OST 池" /> }} />
      <Typography.Title level={2} className="resource-subheading">OST / MDT 发现信息</Typography.Title>
      <Descriptions column={1} items={[
        { key: 'ost', label: 'OST', children: <PoolMembers indices={resource.ostIndices ?? null} /> },
        { key: 'mdt', label: 'MDT', children: resource.mdtNames?.length
          ? <span className="console-mono">{resource.mdtNames.join('、')}</span> : '未取得 MDT 列表' },
      ]} />
    </>
  );
}

function MountInformation({ resource, registration }: { resource: FilesystemResource; registration?: ResourceRegistration }) {
  const { id, mountPath, mountStatus, mount } = resource;
  const source = mount?.source || (mountStatus === 'mounted' ? registration?.source : undefined);
  let autoMount = '由外部负责';
  if (registration?.owner === 'daemon') {
    autoMount = registration.autoMount ? '开启' : '关闭';
    if (registration.autoMount && !registration.desiredMounted) autoMount = '手动挂载后启用';
  }
  return <Descriptions column={{ xs: 1, md: 2 }} items={[
    { key: 'id', label: '文件系统 ID', children: <span className="console-mono">{id}</span> },
    { key: 'path', label: '注册挂载路径', children: <span className="console-mono">{mountPath}</span> },
    { key: 'state', label: '挂载状态', children: <MountStatus status={mountStatus} /> },
    { key: 'source', label: '挂载来源', children: <span className="console-mono">{source || '未取得'}</span> },
    { key: 'mode', label: '访问模式', children: mount === null ? '未取得' : mount.readOnly ? '只读挂载' : '读写挂载' },
    ...(registration ? [
      { key: 'owner', label: '挂载所有权', children: resourceOwnerLabels[registration.owner] },
      { key: 'options', label: '挂载参数', children: registration.options.join(', ') || '默认' },
      { key: 'auto', label: 'daemon 重启后恢复挂载', children: autoMount },
    ] : []),
  ]} />;
}

function ResourceDetails({ id }: { id: string }) {
  const load = useCallback((signal: AbortSignal) => getFilesystem(id, signal), [id]);
  const { state, refresh } = useInventory(load, load, { keepPrevious: true, queueRefresh: true });
  const { management, error: managementError, refresh: reloadManagement } = useResourceManagement();
  useResourceEvents(refresh, reloadManagement, id);
  const registration = management?.resources.find((candidate) => candidate.id === id);
  const [params, setParams] = useSearchParams();
  const requestedTab = params.get('tab');
  const tab = requestedTab === 'storage' || requestedTab === 'mount' ? requestedTab : 'overview';
  const now = useObservationClock();
  const resource = state.status === 'ready' && state.data ? withLiveMountStatus(state.data, registration) : null;

  function refreshResources() {
    refresh();
    reloadManagement();
  }

  return (
    <>
      <Link to="/resources" className="resource-back"><ArrowLeftOutlined /> 返回 Lustre 集群列表</Link>
      <InventoryPage title={registration?.name || id} status={state.status}
        error={state.status === 'error' ? state.message : undefined}
        refreshError={state.status === 'ready' ? state.refreshError : undefined} retry={refreshResources}
        action={registration && resource && <ResourceManagement resource={registration}
          pools={resource?.pools?.map(({ name }) => name)}
          mounted={resource.mountStatus === 'mounted'}
          onRefresh={refreshResources} />}>
        {managementError && <Alert type="warning" showIcon message="集群挂载信息读取失败" description={managementError}
          action={<Button onClick={reloadManagement}>重试</Button>} />}
        {state.status === 'ready' && !resource && <Result status="404" title="Lustre 文件系统未注册" />}
        {resource && (
          <div className="resource-panel">
            {resource.mountStatus !== 'mounted' && registration?.lastMountError && <Alert type="warning" showIcon message="最近挂载失败"
              description={registration.lastMountError} />}
            <Tabs activeKey={tab} onChange={(key) => setParams(key === 'overview' ? { filesystem: id } : { filesystem: id, tab: key })} items={[
              { key: 'overview', label: '集群概览', children: <Overview resource={resource} now={now} /> },
              { key: 'storage', label: '存储资源', children: <Storage resource={resource} now={now} registration={registration} /> },
              { key: 'mount', label: '挂载信息', children: <MountInformation resource={resource} registration={registration} /> },
            ]} />
          </div>
        )}
      </InventoryPage>
    </>
  );
}

export default function ResourceRoute() {
  const [params] = useSearchParams();
  const id = params.get('filesystem');
  return id ? <ResourceDetails key={id} id={id} /> : (
    <Result status="404" title="请选择 Lustre 文件系统" extra={<Link to="/resources">返回 Lustre 集群列表</Link>} />
  );
}
