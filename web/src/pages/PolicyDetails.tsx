import { Link, useParams } from '@umijs/max';
import { Alert, Button, Card, Descriptions, Flex, Tabs, Tag, Typography } from 'antd';
import { useCallback, useEffect, useRef, useState } from 'react';
import InventoryPage from '@/components/InventoryPage';
import { PolicyBindings, PolicyConfiguration } from '@/components/PolicyConfiguration';
import { policyPoolPair, readEditablePolicy } from '@/domain/tieringPolicy';
import { readPolicyDraft } from '@/services/policyDrafts';
import { getPolicy, getPolicyDefinition, policyDetailsPath, policyEditPath, type PolicyRevision } from '@/services/policies';
import { filesystemDetailsPath } from '@/services/tiering';
import { requestError, useInventory } from '@/services/useInventory';

function PolicyJson({ id, revision }: { id: number; revision: number }) {
  const read = useCallback((signal: AbortSignal) => getPolicyDefinition(id, revision, signal), [id, revision]);
  const { state, refresh } = useInventory(read);
  if (state.status === 'error') {
    return <Alert showIcon type="error" message="读取定义失败" description={state.message}
      action={<Button onClick={refresh}>重试</Button>} />;
  }
  if (state.status === 'loading') return <Typography.Text role="status">正在读取 Rev.8 原文</Typography.Text>;
  return (
    <Card size="small" title="Rev.8 原文">
      <pre className="console-mono" style={{ maxHeight: 500, overflow: 'auto', whiteSpace: 'pre-wrap', margin: 0 }}>
        {state.data}
      </pre>
    </Card>
  );
}

function Details({ row, historical }: { row: PolicyRevision; historical: boolean }) {
  const { id, revision, definition } = row;
  const { metadata, spec } = definition;
  const decoded = readEditablePolicy(definition);
  const pair = policyPoolPair(definition);
  const [downloading, setDownloading] = useState(false);
  const [exportError, setExportError] = useState<string>();
  const exportRequest = useRef<AbortController | null>(null);
  useEffect(() => () => exportRequest.current?.abort(), []);

  async function exportDefinition() {
    exportRequest.current?.abort();
    const request = new AbortController();
    exportRequest.current = request;
    setDownloading(true);
    setExportError(undefined);
    try {
      const json = await getPolicyDefinition(id, revision, request.signal);
      if (request.signal.aborted) return;
      const url = URL.createObjectURL(new Blob([json], { type: 'application/json' }));
      const anchor = document.createElement('a');
      anchor.href = url;
      anchor.download = `${metadata.name}-r${revision}.json`;
      anchor.click();
      setTimeout(() => URL.revokeObjectURL(url), 0);
    } catch (error) {
      if (!request.signal.aborted) setExportError(requestError(error));
    } finally {
      if (!request.signal.aborted) setDownloading(false);
    }
  }

  return (
    <Flex vertical gap={16}>
      <Flex gap={12} align="center" wrap>
        <Tag>修订 {revision}</Tag>
        <Tag color={spec.enabled === false ? 'default' : 'success'}>{spec.enabled === false ? '已停用' : '已启用'}</Tag>
        {historical && <Link to={policyDetailsPath(id)}>查看当前修订</Link>}
        {revision > 1 && <Link to={policyDetailsPath(id, revision - 1)}>查看上一修订</Link>}
        {decoded && !historical && <Link to={policyEditPath(id)}>编辑策略</Link>}
        {!decoded && !historical && readPolicyDraft(id) && <Link to={policyEditPath(id)}>查看本地草稿</Link>}
        <Button loading={downloading} onClick={exportDefinition}>导出 Rev.8 JSON</Button>
      </Flex>
      {exportError && <Alert showIcon type="error" message="导出失败" description={exportError} />}
      {!decoded && <Alert showIcon type="info" message="此策略超出向导的可编辑范围，可查看和导出完整定义。" />}
      <Tabs items={[
        {
          key: 'overview', label: '策略概览',
          children: (
            <Card>
              <Descriptions column={{ xs: 1, md: 2 }} items={[
                { key: 'name', label: '策略名称', children: metadata.name },
                { key: 'id', label: '策略 ID', children: id },
                { key: 'description', label: '描述', children: metadata.description || '—' },
                { key: 'actor', label: '保存者', children: row.modifiedBy ?? '—' },
                { key: 'filesystem', label: '文件系统', children: <Link to={filesystemDetailsPath(spec.target.filesystem)}>{spec.target.filesystem}</Link> },
                { key: 'pools', label: '源池 → 目标池', children: pair && `${pair.sourcePool} → ${pair.targetPool}` },
                { key: 'root', label: '起始目录', children: spec.target.directoryScope?.root ?? '未限定目录' },
                { key: 'excluded', label: '排除子目录', children: spec.target.directoryScope?.excludedSubtrees?.join('、') || '无' },
                { key: 'interval', label: '周期', children: spec.trigger.interval ?? '—' },
              ]} />
            </Card>
          ),
        },
        {
          key: 'configuration', label: '策略配置',
          children: (
            <Flex vertical gap={12}>
              {decoded && <PolicyConfiguration values={decoded.values} settings={decoded.settings} />}
              <PolicyBindings bindings={row.resourceBindings} />
              <PolicyJson id={id} revision={revision} />
            </Flex>
          ),
        },
      ]} />
    </Flex>
  );
}

function PolicyRevisionPage({ id, revision }: { id: number; revision?: number }) {
  const load = useCallback((signal: AbortSignal) => getPolicy(id, signal, revision), [id, revision]);
  const { state, refresh } = useInventory(load);
  const title = state.status === 'ready' ? state.data.definition.metadata.name : '策略详情';
  return (
    <InventoryPage title={title} status={state.status} error={state.status === 'error' ? state.message : undefined}
      refresh={state.status === 'error' ? refresh : undefined}
      action={<Link to="/policies">返回策略列表</Link>}>
      {state.status === 'ready' && <Details key={state.data.revision} row={state.data} historical={revision !== undefined} />}
    </InventoryPage>
  );
}

export default function PolicyDetails() {
  const { id, revision } = useParams();
  return <PolicyRevisionPage key={`${id}/${revision}`} id={Number(id)} revision={revision ? Number(revision) : undefined} />;
}
