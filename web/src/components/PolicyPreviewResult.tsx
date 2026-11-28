import { Card, Col, Descriptions, Row, Statistic, Typography } from 'antd';
import type { PolicyPreview } from '@/services/policyPreview';

const reasonLabels: Record<string, string> = {
  link_outside_scope: '链接位于范围外',
  link_in_excluded_subtree: '链接位于排除子树',
  incomplete_link_evidence: '链接证据不完整',
};

function reasonItems(reasons: Record<string, number>) {
  return Object.entries(reasons).map(([reason, count]) => ({
    key: reason, label: reasonLabels[reason] ?? reason, children: count,
  }));
}

export default function PolicyPreviewResult({ preview }: { preview: PolicyPreview }) {
  const scope = preview.directoryScope;
  const excluded = reasonItems(scope?.excludedByReason ?? {});
  const deferred = reasonItems(scope?.deferredByReason ?? {});
  return (
    <section aria-labelledby="policy-preview-result" aria-live="polite">
      <Card title={<span id="policy-preview-result">预览结果</span>}>
        <Row gutter={[24, 12]}>
          <Col xs={24} sm={12}><Statistic title="候选 FID 数" value={preview.candidateCount} /></Col>
          <Col xs={24} sm={12}><Statistic title="估算逻辑字节" value={preview.estimatedBytesExact ?? preview.estimatedBytes} /></Col>
        </Row>
        {scope && (
          <Row gutter={[24, 12]} style={{ marginTop: 20 }}>
            <Col xs={24} lg={12}>
              <Descriptions column={1} size="small" title="范围排除（FID）"
                items={excluded.length ? excluded : [{ key: 'none', label: '合计', children: 0 }]} />
            </Col>
            <Col xs={24} lg={12}>
              <Descriptions column={1} size="small" title="暂缓（FID）"
                items={deferred.length ? deferred : [{ key: 'none', label: '合计', children: 0 }]} />
            </Col>
          </Row>
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
