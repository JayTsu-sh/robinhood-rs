import { DeleteOutlined, PlusOutlined } from '@ant-design/icons';
import { Link } from '@umijs/max';
import { Alert, Button, Card, Col, Flex, Form, Input, Row, Select, Typography } from 'antd';
import { useEffect, useRef, useState } from 'react';
import InventoryPage from '@/components/InventoryPage';
import PolicyPreviewResult from '@/components/PolicyPreviewResult';
import { poolScopePreviewDocument, previewPolicyDocument, type PolicyPreview as Preview, type PreviewScope } from '@/services/policyPreview';
import { listFilesystems, type FilesystemResource } from '@/services/tiering';
import { requestError, useInventory } from '@/services/useInventory';

type PreviewState = { status: 'running' } | { status: 'ready'; data: Preview } | { status: 'error'; message: string };

function PreviewForm({ filesystems }: { filesystems: FilesystemResource[] }) {
  const [form] = Form.useForm<PreviewScope>();
  const filesystem = Form.useWatch('filesystem', form);
  const sourcePool = Form.useWatch('sourcePool', form);
  const pools = filesystems.find(({ id }) => id === filesystem)?.pools?.filter(({ eligible }) => eligible) ?? [];
  const poolOptions = pools.map(({ name }) => ({ value: name, label: name }));
  const request = useRef<AbortController | null>(null);
  const [preview, setPreview] = useState<PreviewState>();

  useEffect(() => () => request.current?.abort(), []);

  function changed(values: Partial<PreviewScope>) {
    request.current?.abort();
    setPreview(undefined);
    if ('filesystem' in values) {
      form.resetFields(['sourcePool', 'targetPool']);
    } else if ('sourcePool' in values && values.sourcePool === form.getFieldValue('targetPool')) {
      form.resetFields(['targetPool']);
    }
  }

  async function submit(values: PreviewScope) {
    request.current?.abort();
    const controller = new AbortController();
    request.current = controller;
    setPreview({ status: 'running' });
    try {
      const data = await previewPolicyDocument(poolScopePreviewDocument(values), controller.signal);
      if (!controller.signal.aborted) setPreview({ status: 'ready', data });
    } catch (error) {
      if (!controller.signal.aborted) setPreview({ status: 'error', message: requestError(error) });
    }
  }

  return (
    <Flex vertical gap={16}>
      <Card>
        <Typography.Paragraph type="secondary">
          目录以所选文件系统根目录为基准，包含其递归子目录。同一文件的所有硬链接均须在允许范围内。
        </Typography.Paragraph>
        <Form
          form={form}
          name="pool-scope-preview"
          layout="vertical"
          initialValues={{ root: '/', excludedSubtrees: [] }}
          onValuesChange={changed}
          onFinish={submit}
          requiredMark={false}
        >
          <Row gutter={16}>
            <Col xs={24} md={8}>
              <Form.Item name="filesystem" label="Lustre 文件系统" rules={[{ required: true, message: '请选择文件系统' }]}>
                <Select placeholder="选择文件系统" options={filesystems.map(({ id }) => ({ value: id, label: id }))} virtual={false} />
              </Form.Item>
            </Col>
            <Col xs={24} md={8}>
              <Form.Item name="sourcePool" label="源池" rules={[{ required: true, message: '请选择源池' }]}>
                <Select placeholder="选择源池" disabled={!filesystem} options={poolOptions} virtual={false} />
              </Form.Item>
            </Col>
            <Col xs={24} md={8}>
              <Form.Item name="targetPool" label="目标池" rules={[{ required: true, message: '请选择目标池' }]}>
                <Select placeholder="选择目标池" disabled={!sourcePool}
                  options={poolOptions.filter(({ value }) => value !== sourcePool)} virtual={false} />
              </Form.Item>
            </Col>
          </Row>
          <Form.Item name="root" label="起始目录" rules={[{ required: true, pattern: /^\//, message: '请输入以 / 开头的起始目录' }]}>
            <Input className="console-mono" placeholder="/team" />
          </Form.Item>
          <Form.List name="excludedSubtrees">
            {(fields, { add, remove }) => (
              <>
                {fields.map(({ key, name }) => (
                  <Form.Item key={key} label={`排除子目录 ${name + 1}`} htmlFor={`exclude-directory-${key}`}>
                    <Flex align="start" gap={8}>
                      <Form.Item name={name} style={{ flex: 1, marginBottom: 0 }}
                        rules={[{ required: true, pattern: /^\//, message: '请输入以 / 开头的排除子目录' }]}>
                        <Input id={`exclude-directory-${key}`} className="console-mono" placeholder="/team/archive" />
                      </Form.Item>
                      <Button icon={<DeleteOutlined />} aria-label={`移除排除子目录 ${name + 1}`} onClick={() => remove(name)} />
                    </Flex>
                  </Form.Item>
                ))}
                <Button icon={<PlusOutlined />} onClick={() => add('')}>添加排除目录</Button>
              </>
            )}
          </Form.List>
          <Flex gap={12} align="center" wrap style={{ marginTop: 24 }}>
            <Button type="primary" htmlType="submit" loading={preview?.status === 'running'}>计算预览</Button>
            <Typography.Text type="secondary">预览基于当前目录索引覆盖的文件。</Typography.Text>
          </Flex>
        </Form>
      </Card>
      {preview?.status === 'error' && <Alert type="error" showIcon message="预览失败" description={preview.message} />}
      {preview?.status === 'ready' && <PolicyPreviewResult preview={preview.data} />}
    </Flex>
  );
}

export default function PolicyPreview() {
  const { state, refresh } = useInventory(listFilesystems);
  const filesystems = state.status === 'ready' ? state.data.filter(({ previewAvailable }) => previewAvailable) : [];
  return (
    <InventoryPage
      title="预览池迁移"
      status={state.status}
      error={state.status === 'error' ? state.message : undefined}
      refresh={filesystems.length ? undefined : refresh}
      action={<Link to="/policies">返回分层策略</Link>}
    >
      {filesystems.length ? <PreviewForm filesystems={filesystems} /> : (
        <Alert type="info" showIcon message="暂无可预览池迁移的文件系统"
          description={<Link to="/resources">查看 Lustre 集群和可用池</Link>} />
      )}
    </InventoryPage>
  );
}
