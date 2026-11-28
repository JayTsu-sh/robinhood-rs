import { Link, useParams, useSearchParams } from '@umijs/max';
import { Alert, Button, Card, Descriptions, Flex, Tabs, Tag } from 'antd';
import { useCallback, useEffect, useRef, useState } from 'react';
import InventoryPage from '@/components/InventoryPage';
import { LatestRunLink } from '@/components/ExecutionStatus';
import { PolicyBindings, PolicyConfiguration } from '@/components/PolicyConfiguration';
import PolicyExecutionActions from '@/components/PolicyExecutionActions';
import PolicyJson from '@/components/PolicyJson';
import RunHistoryTable from '@/components/RunHistoryTable';
import { policyPoolPair, readEditablePolicy } from '@/domain/tieringPolicy';
import { executionFilters, runHistoryPath } from '@/services/executions';
import { readPolicyDraft } from '@/services/policyDrafts';
import { getPolicy, getPolicyDefinition, policyDetailsPath, policyEditPath, type PolicyRevision } from '@/services/policies';
import { filesystemDetailsPath } from '@/services/tiering';
import { requestError, useInventory } from '@/services/useInventory';

function Details({ row, historical, refresh }: { row: PolicyRevision; historical: boolean; refresh: () => void }) {
  const { definition, idExact: id, revisionExact: revision } = row;
  const previous = BigInt(revision) - 1n;
  const { metadata, spec } = definition;
  const decoded = readEditablePolicy(definition);
  const pair = policyPoolPair(definition);
  const [params, setParams] = useSearchParams();
  const selectedTab = params.get('tab');
  const tab = selectedTab === 'configuration' || selectedTab === 'history' ? selectedTab : 'overview';
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

  function openTab(value: string) {
    setParams(executionFilters(params, { tab: value === 'overview' ? undefined : value }));
  }

  return (
    <Flex vertical gap={16}>
      <Flex gap={12} align="center" wrap>
        <Tag>修订 {revision}</Tag>
        <Tag color={spec.enabled === false ? 'default' : 'success'}>{spec.enabled === false ? '已停用' : '已启用'}</Tag>
        {historical && <Link to={policyDetailsPath(id)}>查看当前修订</Link>}
        {previous > 0n && <Link to={policyDetailsPath(id, previous.toString())}>查看上一修订</Link>}
        {decoded && !historical && <Link to={policyEditPath(id)}>编辑策略</Link>}
        {pair && !historical && <PolicyExecutionActions id={id} revision={revision} name={metadata.name}
          enabled={spec.enabled !== false} showToggle latestRun={row.latestRun} onRefresh={refresh} />}
        {!decoded && !historical && readPolicyDraft(id) && <Link to={policyEditPath(id)}>查看本地草稿</Link>}
        <Button loading={downloading} onClick={exportDefinition}>导出 Rev.8 JSON</Button>
      </Flex>
      {exportError && <Alert showIcon type="error" message="导出失败" description={exportError} />}
      {!decoded && <Alert showIcon type="info" message="此策略超出向导的可编辑范围，可查看和导出完整定义。" />}
      <Tabs activeKey={tab} onChange={openTab} items={[
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
                ...(!historical ? [{
                  key: 'run', label: '当前或最近运行',
                  children: <Flex vertical gap={8} align="start">
                    <LatestRunLink run={row.latestRun} />
                    <Button type="link" style={{ padding: 0 }} onClick={() => openTab('history')}>查看执行历史</Button>
                  </Flex>,
                }] : []),
              ]} />
            </Card>
          ),
        },
        {
          key: 'history', label: '执行历史',
          children: <Flex vertical gap={12}>
            <Link to={runHistoryPath(id)}>在执行记录中查看</Link>
            <RunHistoryTable policyId={id} />
          </Flex>,
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

function PolicyRevisionPage({ id, revision }: { id: string; revision?: string }) {
  const load = useCallback((signal: AbortSignal) => getPolicy(id, signal, revision, true), [id, revision]);
  const { state, refresh } = useInventory(load);
  const title = state.status === 'ready' ? state.data.definition.metadata.name : '策略详情';
  return (
    <InventoryPage title={title} status={state.status} error={state.status === 'error' ? state.message : undefined}
      refresh={refresh}
      action={<Link to="/policies">返回策略列表</Link>}>
      {state.status === 'ready' && <Details key={state.data.revisionExact} row={state.data}
        historical={revision !== undefined} refresh={refresh} />}
    </InventoryPage>
  );
}

export default function PolicyDetails() {
  const { id = '', revision } = useParams();
  return <PolicyRevisionPage key={`${id}/${revision}`} id={id} revision={revision} />;
}
