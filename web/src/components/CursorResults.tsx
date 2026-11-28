import { ReloadOutlined } from '@ant-design/icons';
import { Alert, Button, Flex, Spin, Typography } from 'antd';
import { useCallback, useState, type ReactNode } from 'react';
import type { CursorPage } from '@/services/executions';
import { useInventory } from '@/services/useInventory';

export default function CursorResults<T>({
  load, refreshable = false, refreshKey, children,
}: {
  load: (cursor: string | undefined, signal: AbortSignal) => Promise<CursorPage<T>>;
  refreshable?: boolean;
  refreshKey?: number;
  children: (rows: T[]) => ReactNode;
}) {
  const [cursors, setCursors] = useState<string[]>([]);
  const cursor = cursors.at(-1);
  const read = useCallback((signal: AbortSignal) => load(cursor, signal), [cursor, load]);
  const { state, refresh } = useInventory(read, read, { refreshKey });
  const next = state.status === 'ready' ? state.data.nextCursor : null;
  const loading = state.status === 'loading';

  function goToPage(value: string[]) {
    setCursors(value);
    refresh();
  }

  return (
    <>
      {loading && <div className="console-loading" role="status"><Spin /> <span>正在读取</span></div>}
      {state.status === 'error' && <Alert type="error" showIcon message="读取失败" description={state.message}
        action={<Button onClick={refresh}>重试</Button>} />}
      {state.status === 'ready' && children(state.data.items)}
      <Flex className="console-pagination" gap={8} align="center" wrap>
        {refreshable && <Button icon={<ReloadOutlined aria-hidden />} loading={loading} onClick={() => goToPage([])}>刷新列表</Button>}
        <Typography.Text type="secondary">第 {cursors.length + 1} 页</Typography.Text>
        <Button disabled={!cursors.length || loading} onClick={() => goToPage(cursors.slice(0, -1))}>上一页</Button>
        <Button disabled={next === null} onClick={() => { if (next !== null) goToPage([...cursors, next]); }}>下一页</Button>
      </Flex>
    </>
  );
}
