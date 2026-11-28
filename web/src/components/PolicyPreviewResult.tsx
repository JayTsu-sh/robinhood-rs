import { Card, Col, Descriptions, Row, Statistic, Table, Typography } from 'antd';
import type { DirectoryScopeSummary, PolicyPreview } from '@/services/policyPreview';

const reasonLabels: Record<string, string> = {
  link_outside_scope: '链接位于范围外',
  link_in_excluded_subtree: '链接位于排除子树',
  incomplete_link_evidence: '链接证据不完整',
};
const outcomes: Record<string, string> = {
  ...reasonLabels,
  filtered: '未通过纳入／排除条件',
  planned: '计划迁移',
  retained: '命中保留规则',
  no_rule: '无规则匹配',
  condition_unknown: '条件证据不足',
};

function reasonItems(reasons: Record<string, number>) {
  return Object.entries(reasons).map(([reason, count]) => ({
    key: reason, label: reasonLabels[reason] ?? reason, children: count,
  }));
}

export function DirectoryScopeResults({ scope }: { scope: DirectoryScopeSummary }) {
  const excluded = reasonItems(scope.excludedByReason);
  const deferred = reasonItems(scope.deferredByReason);
  return (
    <Row gutter={[24, 12]}>
      <Col xs={24} lg={12}>
        <Descriptions column={1} size="small" title="范围排除（FID）"
          items={excluded.length ? excluded : [{ key: 'none', label: '合计', children: 0 }]} />
      </Col>
      <Col xs={24} lg={12}>
        <Descriptions column={1} size="small" title="范围暂缓（FID）"
          items={deferred.length ? deferred : [{ key: 'none', label: '合计', children: 0 }]} />
      </Col>
    </Row>
  );
}

export default function PolicyPreviewResult({ preview }: { preview: PolicyPreview }) {
  const scope = preview.directoryScope;
  return (
    <section aria-labelledby="policy-preview-result" aria-live="polite">
      <Card title={<span id="policy-preview-result">预览结果</span>}>
        <Row gutter={[24, 12]}>
          <Col xs={24} sm={12}><Statistic title="候选 FID 数" value={preview.candidateCount} /></Col>
          <Col xs={24} sm={12}><Statistic title="估算逻辑字节" value={preview.estimatedBytesExact ?? preview.estimatedBytes} /></Col>
        </Row>
        {scope && <div style={{ marginTop: 20 }}><DirectoryScopeResults scope={scope} /></div>}
        {!!preview.sampleFiles?.length && (
          <>
            <Typography.Title level={3}>索引文件样本</Typography.Title>
            <Table size="small" rowKey="fid" dataSource={preview.sampleFiles} pagination={{ pageSize: 10 }}
              scroll={{ x: 640 }} columns={[
                { title: '路径 / FID', render: (_, { paths, fid }) => <div className="console-mono">
                  {paths.map((path) => <div key={path}>{path}</div>)}<Typography.Text type="secondary">{fid}</Typography.Text>
                </div> },
                { title: '命中规则', dataIndex: 'rule', width: 170, render: (rule) => rule ?? '—' },
                { title: '评估结果', dataIndex: 'outcome', width: 180, render: (outcome) => outcomes[outcome] ?? outcome },
              ]} />
          </>
        )}
        <Descriptions
          style={{ marginTop: 20 }}
          column={1}
          size="small"
          title="资源修订"
          items={preview.resourceBindings.map(({ kind, name, revision }) => ({
            key: `${kind}/${name}`,
            label: kind,
            children: <Typography.Text className="console-mono">{name} · 修订 {revision}</Typography.Text>,
          }))}
        />
      </Card>
    </section>
  );
}
