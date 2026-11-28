import { Alert, Button, Card, Typography } from 'antd';
import { useCallback } from 'react';
import { getPolicyDefinition } from '@/services/policies';
import { useInventory } from '@/services/useInventory';

export default function PolicyJson({ id, revision }: { id: string; revision: string }) {
  const read = useCallback((signal: AbortSignal) => getPolicyDefinition(id, revision, signal), [id, revision]);
  const { state, refresh } = useInventory(read);
  if (state.status === 'error') {
    return <Alert showIcon type="error" message="读取定义失败" description={state.message}
      action={<Button onClick={refresh}>重试</Button>} />;
  }
  if (state.status === 'loading') return <Typography.Text role="status">正在读取 策略原文</Typography.Text>;
  return (
    <Card size="small" title="策略原文">
      <pre className="console-mono" style={{ maxHeight: 500, overflow: 'auto', whiteSpace: 'pre-wrap', margin: 0 }}>
        {state.data}
      </pre>
    </Card>
  );
}
