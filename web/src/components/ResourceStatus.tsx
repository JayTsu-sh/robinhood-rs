import { Tag, Typography } from 'antd';
import { useEffect, useState } from 'react';
import type { Capacity, FilesystemResource } from '@/services/tiering';

const mountLabels = {
  mounted: ['success', '已挂载'],
  unmounted: ['default', '未挂载'],
  unknown: ['warning', '未知'],
} as const;

const collectionLabels = {
  pending: ['default', '等待采集'],
  collected: ['success', '已采集'],
  partial: ['warning', '部分采集'],
  unavailable: ['default', '不可采集'],
  error: ['error', '采集失败'],
} as const;

const capacityLabels = {
  collected: ['success', '已采集'],
  partial: ['warning', '部分缺失'],
  missing: ['default', '未取得容量'],
  error: ['error', '容量采集失败'],
} as const;

const resourceReasons: Record<string, string> = {
  not_a_lustre_mount: '配置路径未挂载 Lustre',
  read_only_mount: '挂载为只读',
  probe_failed: '采集任务失败',
  probe_timed_out: '采集超时，请刷新重试',
  lustre_hsm_capability_required: '当前策略要求注册配置开启 Lustre HSM 能力',
  stripe_and_ost_capabilities_required: '注册配置需开启条带和 OST 能力',
  two_eligible_pools_required: '至少需要两个可用且不同的池',
  pool_has_no_osts: '池内没有 OST',
  unsupported_pool_name: '池名称无法用于分层',
  capacity_bytes_overflow: '容量超出计数范围',
  no_registered_osts: '客户端未登记 OST',
  collection_pending: '正在等待首次资源快照',
};

const observationTime = new Intl.DateTimeFormat('zh-CN', {
  dateStyle: 'medium',
  timeStyle: 'medium',
});
const quantity = new Intl.NumberFormat('zh-CN', { maximumFractionDigits: 2 });
const percent = new Intl.NumberFormat('zh-CN', { style: 'percent', maximumFractionDigits: 2 });
const units = ['B', 'KiB', 'MiB', 'GiB', 'TiB', 'PiB', 'EiB'];

export function formatBytes(bytes: number | undefined) {
  if (bytes === undefined) return '—';
  let unit = 0;
  while (bytes >= 1024 && unit < units.length - 1) {
    bytes /= 1024;
    unit += 1;
  }
  return `${quantity.format(bytes)} ${units[unit]}`;
}

export function formatUsage(ratio: number | undefined) {
  if (ratio === undefined) return '—';
  return ratio > 0 && ratio < 0.0001 ? '<0.01%' : percent.format(ratio);
}

export function formatObservationTime(time: number | null) {
  return time === null ? '—' : observationTime.format(time * 1000);
}

export function useObservationClock() {
  const [now, setNow] = useState(() => Date.now() / 1000);
  useEffect(() => {
    const interval = window.setInterval(() => setNow(Date.now() / 1000), 15_000);
    return () => window.clearInterval(interval);
  }, []);
  return now;
}

export function Reason({ reason }: { reason: string | null }) {
  return reason && (
    <div className="console-status-reason">
      <Typography.Text type="secondary">{resourceReasons[reason] ?? reason}</Typography.Text>
    </div>
  );
}

export function MountStatus({ status }: { status: FilesystemResource['mountStatus'] }) {
  const [color, label] = mountLabels[status];
  return <Tag color={color}>{label}</Tag>;
}

export function CollectionStatus({ status, observedAt, now }: {
  status: FilesystemResource['collectionStatus']; observedAt: number | null; now: number;
}) {
  const [color, label] = collectionLabels[status];
  const stale = observedAt !== null && now - observedAt >= 300;
  return <><Tag color={color}>{label}</Tag>{stale && <Tag color="warning">快照已过期</Tag>}</>;
}

export function Availability({ resource }: { resource: FilesystemResource }) {
  return (
    <>
      <Tag color={resource.tieringAvailable ? 'success' : 'default'}>
        {resource.tieringAvailable ? '可用于分层' : '暂不可用于分层'}
      </Tag>
      <Reason reason={resource.reason} />
    </>
  );
}

export function CapacityStatus({ capacity, now }: { capacity: Capacity; now: number }) {
  const { status, attemptedAt, sample, reason } = capacity;
  const [color, label] = capacityLabels[status];
  const stale = sample && (status !== 'collected' || now - sample.observedAt >= 300);
  return (
    <>
      {stale && <Tag color="warning">容量已过期</Tag>}
      {(!stale || status !== 'collected') && <Tag color={color}>{label}</Tag>}
      {sample && <Reason reason={formatObservationTime(sample.observedAt)} />}
      {status !== 'collected' && attemptedAt != null && (
        <Reason reason={`最近尝试 ${formatObservationTime(attemptedAt)}`} />
      )}
      <Reason reason={reason} />
    </>
  );
}
