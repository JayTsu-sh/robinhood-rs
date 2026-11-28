import { Alert, Button, Flex, Input, Modal, Typography } from 'antd';
import { useEffect, useRef, useState } from 'react';
import { controlRun, type RunControl, type RunDetail } from '@/services/executions';
import { writeError, writerTransportAllowed } from '@/services/policies';
import { requestError } from '@/services/useInventory';
import { isActiveRun } from './ExecutionStatus';

const labels = { pause: '暂停', resume: '恢复', cancel: '取消执行', approve: '批准本次运行' };

export default function RunControls({ run, refresh }: { run: RunDetail; refresh: () => void }) {
  const [action, setAction] = useState<RunControl>();
  const [requiresToken, setRequiresToken] = useState(false);
  const [token, setToken] = useState('');
  const [error, setError] = useState<string>();
  const [busy, setBusy] = useState(false);
  const pending = useRef<AbortController>();
  useEffect(() => () => pending.current?.abort(), []);

  function close() {
    setAction(undefined);
    setRequiresToken(false);
    setToken('');
    setError(undefined);
  }

  async function submit() {
    if (pending.current || !action || (requiresToken && !token.trim())) return;
    const request = new AbortController();
    pending.current = request;
    setBusy(true);
    setError(undefined);
    try {
      await controlRun(run.idExact, action, request.signal, requiresToken ? token.trim() : undefined);
      if (!request.signal.aborted) {
        close();
        refresh();
      }
    } catch (cause) {
      if (request.signal.aborted) return;
      if (writeError(cause)?.status === 401 && writerTransportAllowed()) {
        setRequiresToken(true);
        setToken('');
        if (requiresToken) setError('策略写入凭据未获后端接受');
      } else {
        setError(writeError(cause)?.status === 401 ? '请通过本机回环地址或 HTTPS 连接。' : requestError(cause));
      }
    } finally {
      pending.current = undefined;
      if (!request.signal.aborted) setBusy(false);
    }
  }

  const control = run.state === 'awaiting_approval' ? 'approve' : run.state === 'paused' ? 'resume' : 'pause';
  return (
    <>
      {isActiveRun(run.state) && <Flex gap={8}>
        <Button onClick={() => setAction(control)}>{labels[control]}</Button>
        <Button danger onClick={() => setAction('cancel')}>{labels.cancel}</Button>
      </Flex>}
      <Modal title={action && labels[action]} open={action !== undefined} onCancel={close} onOk={submit}
        okText={action && labels[action]} cancelText="返回" confirmLoading={busy} closable={!busy}
        keyboard={!busy} maskClosable={false} cancelButtonProps={{ disabled: busy }}
        okButtonProps={{ disabled: requiresToken && !token.trim(), danger: action === 'cancel' }} destroyOnHidden>
        <Flex vertical gap={12}>
          <Typography.Text strong>运行 #{run.idExact}</Typography.Text>
          <Typography.Text>{action === 'approve' ? '批准当前策略修订的本次执行，后端仍会检查授权、时间窗口和文件资格。' : action === 'resume'
            ? '继续派发此运行的待处理操作。'
            : '停止派发新的操作。已提交的操作继续核对。'}</Typography.Text>
          {requiresToken && <label>
            策略写入令牌
            <Input.Password autoComplete="off" disabled={busy} value={token}
              onChange={(event) => setToken(event.target.value)} onPressEnter={submit} />
          </label>}
          {error && <Alert type="error" showIcon message={error} />}
        </Flex>
      </Modal>
    </>
  );
}
