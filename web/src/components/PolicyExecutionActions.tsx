import { Link, useNavigate } from '@umijs/max';
import { Alert, Button, Flex, Input, Modal, Typography } from 'antd';
import { useEffect, useRef, useState } from 'react';
import { runDetailsPath, type LatestRun } from '@/services/executions';
import { requestPolicyRun, setPolicyEnabled, writeError, writerTransportAllowed } from '@/services/policies';
import { requestError } from '@/services/useInventory';

const labels = { enable: '启用策略', disable: '停用策略', run: '立即运行' };
type Action = keyof typeof labels;

interface Props {
  id: string;
  revision: string;
  name: string;
  enabled: boolean;
  latestRun?: LatestRun;
  showToggle?: boolean;
  onRefresh: () => void;
}

export default function PolicyExecutionActions(
  { id, revision, name, enabled, latestRun, showToggle, onRefresh }: Props,
) {
  const [action, setAction] = useState<Action>();
  const [saving, setSaving] = useState(false);
  const [requiresToken, setRequiresToken] = useState(false);
  const [token, setToken] = useState('');
  const [error, setError] = useState<string>();
  const [conflict, setConflict] = useState<string>();
  const [existingRun, setExistingRun] = useState<{ id: string; active: boolean }>();
  const pending = useRef<AbortController>();
  const navigate = useNavigate();
  useEffect(() => () => pending.current?.abort(), []);

  const active = latestRun?.revision === revision && (latestRun.state === 'queued' || latestRun.state === 'running');
  const explanation = action === 'disable'
    ? '停用后，旧修订将停止派发新操作。已提交的操作会继续核对。'
    : action === 'enable'
      ? '启用会保存一个新修订，并按策略配置的周期调度。'
      : '提交当前修订的一次运行，使用策略的执行预算与准入条件。';

  function close() {
    setAction(undefined);
    setRequiresToken(false);
    setToken('');
    setError(undefined);
    setConflict(undefined);
    setExistingRun(undefined);
  }

  async function submit() {
    if (pending.current || !action || conflict || existingRun || (requiresToken && !token.trim())) return;
    const request = new AbortController();
    pending.current = request;
    setSaving(true);
    setError(undefined);
    try {
      const base = { id, revision };
      const writer = requiresToken ? token.trim() : undefined;
      if (action === 'run') {
        const { runIdExact } = await requestPolicyRun(base, writer, request.signal);
        if (!request.signal.aborted) navigate(runDetailsPath(runIdExact));
      } else {
        await setPolicyEnabled(base, action === 'enable', writer, request.signal);
        if (!request.signal.aborted) onRefresh();
      }
      if (!request.signal.aborted) close();
    } catch (cause) {
      if (request.signal.aborted) return;
      const response = writeError(cause);
      if (response?.status === 401 && writerTransportAllowed()) {
        setRequiresToken(true);
        setToken('');
        if (requiresToken) setError('策略写入凭据未获后端接受');
      } else {
        let message = response?.status === 401
          ? '请通过本机回环地址或 HTTPS 连接后提交策略写入请求。'
          : requestError(cause);
        if (response?.status === 409) {
          setConflict(response.data?.currentRevisionExact);
          const activeId = response.data?.activeRunIdExact;
          const previousId = response.data?.existingRunIdExact;
          if (activeId) {
            setExistingRun({ id: activeId, active: true });
            message = '当前修订已有运行在排队或派发。';
          } else if (previousId) {
            setExistingRun({ id: previousId, active: false });
            message = '一次性策略已有执行记录。';
          }
        }
        setError(message);
      }
    } finally {
      pending.current = undefined;
      if (!request.signal.aborted) setSaving(false);
    }
  }

  return (
    <Flex gap={12} align="center" wrap>
      {showToggle && <Button onClick={() => setAction(enabled ? 'disable' : 'enable')}>
        {enabled ? labels.disable : labels.enable}
      </Button>}
      {enabled && active
        ? <Link to={runDetailsPath(latestRun.id)}>查看当前运行</Link>
        : <Button type={showToggle ? 'default' : 'link'} style={showToggle ? undefined : { padding: 0 }}
            disabled={!enabled} onClick={() => setAction('run')}>{labels.run}</Button>}
      <Modal title={action && labels[action]} open={action !== undefined} onCancel={close} onOk={submit}
        okText={action && labels[action]} cancelText="取消" confirmLoading={saving}
        closable={!saving} keyboard={!saving} maskClosable={false} cancelButtonProps={{ disabled: saving }}
        okButtonProps={{ disabled: !!conflict || !!existingRun || (requiresToken && !token.trim()) }} destroyOnHidden>
        <Flex vertical gap={12}>
          <Typography.Text strong>{name} · 修订 {revision}</Typography.Text>
          <Typography.Text>{explanation}</Typography.Text>
          {requiresToken && <label>
            策略写入令牌
            <Input.Password autoComplete="off" disabled={saving} value={token}
              onChange={(event) => setToken(event.target.value)} onPressEnter={submit} />
          </label>}
          {error && <Alert showIcon type="error" message={error} action={conflict
            ? <Button onClick={() => { close(); onRefresh(); }}>重新读取策略</Button>
            : existingRun ? <Link to={runDetailsPath(existingRun.id)}>
              {existingRun.active ? '查看当前运行' : '查看执行记录'}
            </Link> : undefined} />}
        </Flex>
      </Modal>
    </Flex>
  );
}
