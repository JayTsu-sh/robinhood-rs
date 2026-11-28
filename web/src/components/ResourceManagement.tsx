import { PlusOutlined } from '@ant-design/icons';
import { Alert, Button, Checkbox, Flex, Form, Input, Modal, Select, Space, Typography } from 'antd';
import { useEffect, useRef, useState } from 'react';
import {
  listResourceManagement, poolTiers, writeResource, type PoolTier, type ResourceRegistration,
} from '@/services/resourceManagement';
import { writeError, writerTransportAllowed } from '@/services/policies';
import { requestError, useInventory } from '@/services/useInventory';

export function useResourceManagement() {
  const { state, refresh } = useInventory(listResourceManagement, listResourceManagement, {
    keepPrevious: true, queueRefresh: true,
  });
  return {
    management: state.status === 'ready' ? state.data : undefined,
    error: state.status === 'error' ? state.message : state.status === 'ready' ? state.refreshError : undefined,
    refresh,
  };
}

type Management = Awaited<ReturnType<typeof listResourceManagement>>;
type Action = 'add' | 'rename' | 'set_tier' | 'set_auto_mount' | 'mount' | 'unmount' | 'remove';
const labels: Record<Action, string> = {
  add: '添加集群', rename: '编辑集群名称', set_tier: '编辑 Pool 层级', set_auto_mount: '设置自动恢复',
  mount: '挂载', unmount: '卸载', remove: '移除接入',
};

type Values = {
  id: string;
  name: string;
  mode: 'existing' | 'managed';
  path?: string;
  filesystem?: string;
  nids?: string;
  options?: string;
  autoMount?: boolean;
  mountNow?: boolean;
  pool?: string;
  tier?: PoolTier | 'none';
};

interface Props {
  management: Management;
  resource?: ResourceRegistration;
  pools?: string[];
  mounted?: boolean;
  onRefresh: (removed?: boolean) => void;
  canAdd?: boolean;
  onRemoving?: (resource: ResourceRegistration | undefined) => void;
}

export default function ResourceManagement({
  management, resource, pools = [], mounted, onRefresh, canAdd = true, onRemoving,
}: Props) {
  const [form] = Form.useForm<Values>();
  const mode = Form.useWatch('mode', form) ?? 'existing';
  const [action, setAction] = useState<Action>();
  const [operation, setOperation] = useState<'check' | 'save'>();
  const [retry, setRetry] = useState<'check' | 'save'>();
  const [token, setToken] = useState('');
  const [error, setError] = useState<string>();
  const [notice, setNotice] = useState<string>();
  const pending = useRef<AbortController>();
  useEffect(() => () => pending.current?.abort(), []);

  function open(value: Action) {
    form.resetFields();
    form.setFieldsValue({
      id: '', name: resource?.name ?? '', mode: 'existing', autoMount: resource?.autoMount ?? true, mountNow: true,
    });
    setAction(value);
  }

  function close() {
    setAction(undefined);
    setToken('');
    setRetry(undefined);
    setError(undefined);
    setNotice(undefined);
  }

  async function submit(command: 'check' | 'save') {
    if (!action || pending.current || (retry && !token.trim())) return;
    let values;
    try {
      values = await form.validateFields();
    } catch { return; }
    if (pending.current) return;
    const path = action === 'add' ? (command === 'check' ? '/check' : '') : `/${encodeURIComponent(resource!.id)}`;
    const data = action === 'add' ? {
      id: values.id, name: values.name,
      mount: values.mode === 'existing' ? { type: 'existing', path: values.path } : {
        type: 'managed', filesystem: values.filesystem, mgsNids: values.nids?.trim(), path: values.path?.trim() || undefined,
        options: values.options?.split(',').map((value) => value.trim()).filter(Boolean), autoMount: values.autoMount,
        mountNow: values.mountNow,
      },
    } : {
      action,
      ...(action === 'rename' && { name: values.name }),
      ...(action === 'set_tier' && { pool: values.pool, tier: values.tier === 'none' ? null : values.tier }),
      ...(action === 'set_auto_mount' && { enabled: values.autoMount }),
    };
    const request = new AbortController();
    pending.current = request;
    if (action === 'remove') onRemoving?.(resource);
    setOperation(command);
    setNotice(undefined);
    setError(undefined);
    try {
      await writeResource(path, data, request.signal, retry ? token.trim() : undefined);
      if (request.signal.aborted) return;
      if (command === 'check') {
        setRetry(undefined);
        setToken('');
        setNotice(values.mode === 'existing' ? '已确认本机 Lustre 挂载。' : 'MGS NID 可达。保存时将尝试挂载文件系统。');
      } else {
        close();
        onRefresh(action === 'remove');
      }
    } catch (cause) {
      if (request.signal.aborted) return;
      if (writeError(cause)?.status === 401 && writerTransportAllowed()) {
        if (retry) setError('写入凭据未获后端接受');
        setRetry(command);
        setToken('');
      } else {
        setError(requestError(cause));
      }
    } finally {
      pending.current = undefined;
      if (!request.signal.aborted) {
        if (action === 'remove') onRemoving?.(undefined);
        setOperation(undefined);
      }
    }
  }

  return (
    <>
      {resource ? <Space wrap>
        <Button onClick={() => open('rename')}>{labels.rename}</Button>
        {pools.length > 0 && <Button onClick={() => open('set_tier')}>{labels.set_tier}</Button>}
        {resource.owner === 'daemon' && <>
          <Button onClick={() => open('set_auto_mount')}>{labels.set_auto_mount}</Button>
          <Button onClick={() => open(mounted ? 'unmount' : 'mount')}>{mounted ? labels.unmount : labels.mount}</Button>
        </>}
        {resource.owner !== 'deployment' && <Button danger onClick={() => open('remove')}>{labels.remove}</Button>}
      </Space> : canAdd && <Button type="primary" icon={<PlusOutlined aria-hidden />} onClick={() => open('add')}>添加集群</Button>}
      <Modal title={action && labels[action]} open={action !== undefined} onCancel={close}
        closable={!operation} maskClosable={false} keyboard={!operation} destroyOnHidden
        footer={<Flex align="center" gap={8} justify="end">
          {action === 'add' && !retry && <Button disabled={!!operation} loading={operation === 'check'}
            onClick={() => submit('check')}>{mode === 'existing' ? '检查挂载' : '测试连接'}</Button>}
          <Button onClick={close} disabled={!!operation}>返回</Button>
          <Button type="primary" danger={action === 'remove' || action === 'unmount'}
            loading={!!operation} disabled={!!retry && !token.trim()}
            onClick={() => submit(retry ?? 'save')}>{retry ? '验证并继续' : action === 'add' ? '添加' : '确认'}</Button>
        </Flex>}>
        <Form form={form} layout="vertical" disabled={!!operation} onValuesChange={() => setNotice(undefined)} preserve={false}>
          {action === 'add' && <>
            <Form.Item name="mode" label="接入方式" rules={[{ required: true }]}>
              <Select virtual={false} options={[
                { value: 'existing', label: '接入已有挂载' },
                { value: 'managed', label: '创建新挂载', disabled: !management.canCreateMount },
              ]} onChange={() => { form.resetFields(['path']); }} />
            </Form.Item>
            <Form.Item name="id" label="文件系统 ID" rules={[{ required: true, pattern: /^[A-Za-z0-9_.-]{1,64}$/ }]}>
              <Input />
            </Form.Item>
          </>}
          {(action === 'add' || action === 'rename') && <Form.Item name="name" label="集群名称" rules={[{ required: true, whitespace: true }]}>
            <Input maxLength={255} />
          </Form.Item>}
          {action === 'add' && (mode === 'existing'
            ? <Form.Item key="existing-path" name="path" label="本机 Lustre 挂载" rules={[{ required: true }]}>
              <Select virtual={false} options={management.availableMounts.map(({ path, source }) => ({ value: path, label: `${path} · ${source}` }))}
                notFoundContent="未发现未注册的 Lustre 挂载" />
            </Form.Item>
            : <>
              <Form.Item name="filesystem" label="Lustre 文件系统名称" rules={[{ required: true, pattern: /^[A-Za-z0-9_-]{1,8}$/ }]}><Input /></Form.Item>
              <Form.Item name="nids" label="MGS NID" rules={[{ required: true }]}><Input placeholder="10.0.0.10@tcp" /></Form.Item>
              <Form.Item key="managed-path" name="path" label="挂载路径"><Input placeholder="留空时使用部署挂载目录下的文件系统 ID" /></Form.Item>
              <Form.Item name="options" label="挂载参数"><Input placeholder="例如 flock,noatime" /></Form.Item>
              <Form.Item name="mountNow" valuePropName="checked"><Checkbox>添加后立即挂载</Checkbox></Form.Item>
              <Form.Item name="autoMount" valuePropName="checked"><Checkbox>daemon 重启后恢复挂载</Checkbox></Form.Item>
            </>)}
          {action === 'set_tier' && <>
            <Form.Item name="pool" label="Pool" rules={[{ required: true }]}>
              <Select virtual={false} options={pools.map((pool) => ({ value: pool, label: pool }))}
                onChange={(pool) => form.setFieldValue('tier', resource?.tiers[pool] ?? 'none')} />
            </Form.Item>
            <Form.Item name="tier" label="应用侧层级标签" rules={[{ required: true }]}>
              <Select virtual={false} options={[{ value: 'none', label: '未分类' }, ...Object.entries(poolTiers).map(([value, label]) => ({ value, label }))]} />
            </Form.Item>
          </>}
          {action === 'set_auto_mount' && <Form.Item name="autoMount" valuePropName="checked">
            <Checkbox>daemon 重启后恢复挂载</Checkbox>
          </Form.Item>}
          {action === 'unmount' && <Typography.Paragraph>卸载前需停用关联策略，并等待文件操作和扫描结束。</Typography.Paragraph>}
          {action === 'remove' && <Typography.Paragraph>移除控制台接入，保留本机挂载和执行历史。</Typography.Paragraph>}
          {retry && <label>写入令牌<Input.Password disabled={!!operation} value={token} autoComplete="off"
            onChange={(event) => setToken(event.target.value)} /></label>}
        </Form>
        {error && <Alert type="error" showIcon message={error} />}
        {notice && <Alert type="success" showIcon message={notice} />}
      </Modal>
    </>
  );
}
