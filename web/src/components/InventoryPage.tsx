import { ReloadOutlined } from '@ant-design/icons';
import { Alert, Button, Flex, Spin, Typography } from 'antd';
import type { ReactNode } from 'react';

interface Props {
  title: string;
  status: 'loading' | 'ready' | 'error';
  error?: string;
  refresh?: () => void;
  children: ReactNode;
  action?: ReactNode;
}

export default function InventoryPage({ title, status, error, refresh, children, action }: Props) {
  return (
    <section>
      <div className="console-title">
        <Typography.Title level={1}>{title}</Typography.Title>
        <Flex gap={12} align="center">
          {action}
          {refresh && <Button icon={<ReloadOutlined />} loading={status === 'loading'} onClick={refresh}>刷新</Button>}
        </Flex>
      </div>
      {status === 'error' && (
        <Alert
          type="error"
          showIcon
          message="读取失败"
          description={error}
          action={refresh && <Button onClick={refresh}>重试</Button>}
        />
      )}
      {status === 'loading' && (
        <div className="console-loading" role="status">
          <Spin /> <span>正在读取</span>
        </div>
      )}
      {status === 'ready' && children}
    </section>
  );
}
