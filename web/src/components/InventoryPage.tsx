import { ReloadOutlined } from '@ant-design/icons';
import { Alert, Button, Flex, Spin, Typography } from 'antd';
import type { ReactNode } from 'react';

interface Props {
  title: string;
  description?: string;
  status: 'loading' | 'ready' | 'error';
  error?: string;
  refreshError?: string;
  refresh?: () => void;
  retry?: () => void;
  refreshLabel?: string;
  refreshing?: boolean;
  children: ReactNode;
  action?: ReactNode;
  primaryAction?: ReactNode;
}

export default function InventoryPage({
  title, description, status, error, refreshError, refresh, retry = refresh, refreshLabel = '刷新', refreshing,
  children, action, primaryAction,
}: Props) {
  return (
    <section>
      <div className="console-title">
        {description ? <div>
          <Typography.Title level={1}>{title}</Typography.Title>
          <Typography.Text type="secondary">{description}</Typography.Text>
        </div> : <Typography.Title level={1}>{title}</Typography.Title>}
        <Flex gap={12} align="center" wrap>
          {action}
          {refresh && (
            <Button icon={<ReloadOutlined aria-hidden />} loading={status === 'loading' || refreshing} onClick={refresh}>
              {refreshLabel}
            </Button>
          )}
          {primaryAction}
        </Flex>
      </div>
      {status === 'error' && (
        <Alert
          type="error"
          showIcon
          message="读取失败"
          description={error}
          action={retry && <Button onClick={retry}>重试</Button>}
        />
      )}
      {status === 'ready' && refreshError && <Alert type="warning" showIcon message="资源快照读取失败"
        description={refreshError} action={retry && <Button onClick={retry}>重试</Button>} />}
      {status === 'loading' && (
        <div className="console-loading" role="status">
          <Spin /> <span>正在读取</span>
        </div>
      )}
      {status === 'ready' && children}
    </section>
  );
}
