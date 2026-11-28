import { Card, Col, Row, Statistic } from 'antd';

export default function InventoryStatistics({ items }: { items: [string, number | string][] }) {
  return (
    <Row gutter={[12, 12]} style={{ marginBottom: 16 }}>
      {items.map(([title, value]) => (
        <Col key={title} xs={12} lg={items.length > 4 ? 4 : 6}>
          <Card size="small"><Statistic title={title} value={value} /></Card>
        </Col>
      ))}
    </Row>
  );
}
