import { history, Link, useParams } from '@umijs/max';
import { Alert, Button, Card, Col, Flex, Form, Input, Modal, Row, Steps, Tag, Typography } from 'antd';
import { useCallback, useEffect, useRef, useState } from 'react';
import InventoryPage from '@/components/InventoryPage';
import { PolicyConfiguration } from '@/components/PolicyConfiguration';
import PolicyPreviewResult from '@/components/PolicyPreviewResult';
import PolicyWizardFields from '@/components/PolicyWizardFields';
import {
  newWizardValues, policyDocument, readEditablePolicy, steps, validateStep,
  type FieldIssue, type PolicySettings, type WizardValues,
} from '@/domain/tieringPolicy';
import { previewPolicyDocument, type PolicyPreview } from '@/services/policyPreview';
import { readPolicyDraft, removePolicyDraft, savePolicyDraft, type PolicyDraft } from '@/services/policyDrafts';
import {
  getPolicy, loadWizardResources, policyDetailsPath, savePolicy, writeError, writerTransportAllowed,
  type SavedRevision, type WizardResources,
} from '@/services/policies';
import { requestError, useInventory } from '@/services/useInventory';

type PreviewState = { loading: true } | { data: PolicyPreview } | { error: string };
type SaveFailure = { message: string; conflict?: string };

function Editor({ initial, settings: initialSettings, base: initialBase, resources, reload, currentRevision, exitRecovery }: {
  initial: WizardValues; settings: PolicySettings; base?: SavedRevision; resources: WizardResources;
  reload: (signal: AbortSignal) => Promise<void>; currentRevision?: string; exitRecovery?: () => void;
}) {
  const [form] = Form.useForm();
  const [values, setValues] = useState(initial);
  const [base, setBase] = useState(initialBase);
  const [settings, setSettings] = useState(initialSettings);
  const [step, setStep] = useState(0);
  const [confirmed, setConfirmed] = useState(0);
  const [dirty, setDirty] = useState(!!exitRecovery);
  const [draft, setDraft] = useState(() => readPolicyDraft(initialBase?.id));
  const [draftNotice, setDraftNotice] = useState<{ type: 'success' | 'error'; text: string }>();
  const [invalid, setInvalid] = useState<FieldIssue[]>([]);
  const [preview, setPreview] = useState<PreviewState>();
  const [failure, setFailure] = useState<SaveFailure>();
  const [reloadError, setReloadError] = useState<string>();
  const [reloading, setReloading] = useState(false);
  const [saving, setSaving] = useState(false);
  const [writerPrompt, setWriterPrompt] = useState(false);
  const [writerToken, setWriterToken] = useState('');
  const previewRequest = useRef<AbortController | null>(null);
  const reloadRequest = useRef<AbortController | null>(null);
  const submitting = useRef(false);
  const allowNavigation = useRef(false);

  useEffect(() => () => {
    previewRequest.current?.abort();
    reloadRequest.current?.abort();
  }, []);
  useEffect(() => {
    if (!dirty && !saving) return;
    const unblock = history.block((transition) => {
      if (!allowNavigation.current) {
        if (submitting.current || !window.confirm('表单尚未保存为策略。确定离开向导？')) return;
      }
      unblock();
      transition.retry();
    });
    return unblock;
  }, [dirty, saving]);

  useEffect(() => {
    if (!invalid.length) return;
    form.setFields(invalid.map(({ name, message }) => ({ name, errors: [message] })));
    form.scrollToField(invalid[0].name, { block: 'nearest', focus: true });
  }, [form, invalid, step]);

  function invalidatePreview() {
    previewRequest.current?.abort();
    setPreview(undefined);
  }

  function readValues(): WizardValues {
    return form.getFieldsValue(true) as WizardValues;
  }

  function changed(change: Partial<WizardValues>) {
    if ('filesystem' in change) {
      form.setFieldsValue({ filesystem: change.filesystem ?? '', sourcePool: '', targetPool: '' });
    } else if ('sourcePool' in change && change.sourcePool === form.getFieldValue('targetPool')) {
      form.setFieldValue('targetPool', '');
    }
    setValues(readValues());
    setDirty(true);
    setDraftNotice(undefined);
    form.setFields(invalid.map(({ name }) => ({ name, errors: [] })));
    setInvalid([]);
    setConfirmed((previous) => Math.min(previous, step));
    if (!failure?.conflict) setFailure(undefined);
    invalidatePreview();
  }

  function validate(config: WizardValues, stages: number[]) {
    for (const stage of stages) {
      const issues = validateStep(steps[stage].id, config, resources, settings);
      if (issues.length) {
        setStep(stage);
        setConfirmed((previous) => Math.min(previous, stage));
        setInvalid(issues);
        invalidatePreview();
        return false;
      }
    }
    setInvalid([]);
    return true;
  }

  function next() {
    const config = readValues();
    if (validate(config, [step])) {
      setValues(config);
      setConfirmed((previous) => Math.max(previous, step + 1));
      setStep(step + 1);
    }
  }

  function storeDraft() {
    const nextDraft: PolicyDraft = { version: 1, values: readValues(), base, settings };
    try {
      savePolicyDraft(nextDraft);
      setDraft(nextDraft);
      setDraftNotice({ type: 'success', text: '草稿已保存到当前浏览器' });
    } catch {
      setDraftNotice({ type: 'error', text: '浏览器无法保存草稿' });
    }
  }

  function replace(config: WizardValues, policy: PolicySettings, revision: SavedRevision | undefined, modified: boolean) {
    form.resetFields();
    form.setFieldsValue(config);
    setValues(config);
    setBase(revision);
    setSettings(policy);
    setStep(0);
    setConfirmed(0);
    setDirty(modified);
    setInvalid([]);
    setFailure(undefined);
    setReloadError(undefined);
    invalidatePreview();
  }

  function restoreDraft() {
    if (!draft || (dirty && !window.confirm('用浏览器中的草稿替换当前表单？'))) return;
    replace(draft.values, draft.settings, draft.base, true);
    setDraftNotice({ type: 'success', text: '已恢复本地草稿，请重新确认各步骤' });
  }

  function clearDraft() {
    if (!draft) return;
    try {
      removePolicyDraft(draft);
    } catch {
      setDraftNotice({ type: 'error', text: '浏览器无法清除草稿' });
    }
    setDraft(null);
  }

  function reset() {
    if (!window.confirm('重置当前表单并清除本地草稿？')) return;
    replace(initial, initialSettings, initialBase, false);
    setDraftNotice({ type: 'success', text: '表单已重置' });
    clearDraft();
    exitRecovery?.();
  }

  async function reloadCurrent() {
    if (reloadRequest.current || !window.confirm('重新读取当前修订并替换表单？')) return;
    const request = new AbortController();
    reloadRequest.current = request;
    setReloading(true);
    setReloadError(undefined);
    try {
      await reload(request.signal);
    } catch (error) {
      if (!request.signal.aborted) setReloadError(requestError(error));
    } finally {
      reloadRequest.current = null;
      if (!request.signal.aborted) setReloading(false);
    }
  }

  async function runPreview() {
    const config = readValues();
    if (!validate(config, [0, 1, 2, 3, 4])) return;
    invalidatePreview();
    const controller = new AbortController();
    previewRequest.current = controller;
    setPreview({ loading: true });
    try {
      const data = await previewPolicyDocument(policyDocument(config, settings), controller.signal);
      if (!controller.signal.aborted) setPreview({ data });
    } catch (error) {
      if (!controller.signal.aborted) setPreview({ error: requestError(error) });
    }
  }

  async function submit(token?: string) {
    if (submitting.current) return;
    const config = readValues();
    if (!validate(config, [0, 1, 2, 3, 4])) return;
    submitting.current = true;
    setSaving(true);
    setFailure(undefined);
    try {
      const saved = await savePolicy(policyDocument(config, settings), base, token);
      clearDraft();
      allowNavigation.current = true;
      history.replace(policyDetailsPath(saved.idExact));
    } catch (error) {
      const response = writeError(error);
      if (response?.status === 401) {
        if (writerTransportAllowed()) {
          setWriterPrompt(true);
          if (token) setFailure({ message: 'Rev.8 writer 凭据未获后端接受' });
        } else {
          setFailure({ message: '请通过本机回环地址或 HTTPS 连接后使用 Rev.8 writer。' });
        }
      } else {
        setWriterPrompt(false);
        const conflict = response?.status === 409 ? response.data?.currentRevisionExact : undefined;
        const message = requestError(error);
        setFailure({
          message, conflict,
        });
        const pool = response?.data?.field;
        const field = pool === 'sourcePool' || pool === 'targetPool' ? pool :
          response?.status === 409 && !conflict ? 'name' : undefined;
        if (field) {
          setStep(0);
          setConfirmed(0);
          setInvalid([{ name: [field], message }]);
          invalidatePreview();
        }
      }
    } finally {
      submitting.current = false;
      setSaving(false);
      setWriterToken('');
    }
  }

  const canRestore = draft && String(draft.base?.id ?? '') === String(initialBase?.id ?? '');
  const stale = base && currentRevision !== undefined && String(base.revision) !== currentRevision;
  const validationMessage = invalid.find(({ name }) => name.length === 1 && name[0] === 'rules');
  const busy = saving || reloading;
  return (
    <Flex vertical gap={16}>
      <Card>
        <Steps current={step} onChange={(index) => { setStep(index); setInvalid([]); }}
          items={steps.map(({ title }, index) => ({
            title, disabled: index > confirmed || busy, status: index < confirmed && index !== step ? 'finish' : undefined,
          }))} />
      </Card>
      <Flex gap={8} wrap align="center">
        {initialBase && <Tag>编辑修订 {base?.revision}</Tag>}
        <Button disabled={busy} onClick={storeDraft}>保存草稿</Button>
        {canRestore && <Button disabled={busy} onClick={restoreDraft}>恢复草稿</Button>}
        <Button disabled={busy} onClick={reset}>重置表单</Button>
        {draftNotice && <Typography.Text role="status" type={draftNotice.type === 'error' ? 'danger' : 'success'}>{draftNotice.text}</Typography.Text>}
      </Flex>
      {stale && <Alert type="warning" showIcon message={`草稿基于修订 ${base.revision}，当前修订为 ${currentRevision}`} />}
      {failure && (
        <Alert showIcon type="error"
          message={failure.conflict ? `修订冲突 · 当前修订 ${failure.conflict}` : '保存失败'}
          description={failure.message}
          action={failure.conflict && <Button loading={reloading} onClick={reloadCurrent}>重新加载</Button>} />
      )}
      {reloadError && <Alert showIcon type="error" message="重新加载失败" description={reloadError} />}
      <Row gutter={[16, 16]}>
        <Col xs={24} xl={step < 5 ? 16 : 24}>
          <Card title={<Typography.Title level={2} style={{ fontSize: 18, margin: 0 }}>{steps[step].title}</Typography.Title>}>
            <Form form={form} name="tiering-policy-wizard" layout="vertical" initialValues={initial}
              onValuesChange={changed} disabled={busy}>
              <PolicyWizardFields step={steps[step].id} resources={resources} settings={settings}
                allowScopeToggle={!initial.scoped && !settings.enabled} />
            </Form>
            {validationMessage && <Alert type="error" showIcon message={validationMessage.message} />}
            {step === 5 && (
              <Flex vertical gap={16}>
                <PolicyConfiguration values={values} settings={settings} />
                <Alert type="info" showIcon message={base ? `保存时保留当前${settings.enabled ? '启用' : '停用'}状态` : '新策略保存为停用状态'} />
                {preview && 'error' in preview && <Alert type="error" showIcon message="预览失败" description={preview.error} />}
                {preview && 'data' in preview && <PolicyPreviewResult preview={preview.data} />}
              </Flex>
            )}
            <Flex justify="space-between" gap={8} wrap style={{ marginTop: 18 }}>
              <Button disabled={step === 0 || busy} onClick={() => { setInvalid([]); setStep(step - 1); }}>上一步</Button>
              {step < 5 ? <Button disabled={busy} type="primary" onClick={next}>下一步</Button> : (
                <Flex gap={8} wrap>
                  <Button disabled={busy} loading={!!preview && 'loading' in preview} onClick={runPreview}>计算预览</Button>
                  <Button disabled={reloading} type="primary" loading={saving} onClick={() => submit()}>保存策略</Button>
                </Flex>
              )}
            </Flex>
          </Card>
        </Col>
        {step < 5 && (
          <Col xs={24} xl={8}>
            <Typography.Title level={3} style={{ fontSize: 16 }}>已确认配置</Typography.Title>
            <PolicyConfiguration values={values} settings={settings} confirmed={confirmed} />
          </Col>
        )}
      </Row>
      <Modal title="Rev.8 writer 凭据" open={writerPrompt} confirmLoading={saving} okText="提交保存"
        onCancel={() => { if (!saving) { setWriterPrompt(false); setWriterToken(''); } }}
        okButtonProps={{ disabled: !writerToken.trim() }}
        onOk={() => submit(writerToken.trim())} destroyOnHidden>
        <Typography.Paragraph>输入部署配置中已有的 writer token。凭据用于本次提交。</Typography.Paragraph>
        <Input.Password aria-label="Rev.8 writer token" autoComplete="off" value={writerToken}
          onChange={({ target }) => setWriterToken(target.value)} />
        {failure && <Typography.Paragraph type="danger" role="alert">{failure.message}</Typography.Paragraph>}
      </Modal>
    </Flex>
  );
}

function PolicyWizardPage({ id }: { id?: string }) {
  const load = useCallback(async (signal: AbortSignal) => {
    const [resources, row] = await Promise.all([
      loadWizardResources(signal), id ? getPolicy(id, signal) : Promise.resolve(undefined),
    ]);
    return { resources, row, editable: row ? readEditablePolicy(row.definition) : null };
  }, [id]);
  const { state, refresh } = useInventory(load);
  const [recover, setRecover] = useState(false);
  const [reloaded, setReloaded] = useState<Awaited<ReturnType<typeof load>>>();
  async function reload(signal: AbortSignal) {
    const next = await load(signal);
    if (!signal.aborted) {
      setRecover(false);
      setReloaded(next);
    }
  }
  const data = reloaded ?? (state.status === 'ready' ? state.data : null);
  const row = data?.row;
  const editable = data?.editable;
  const draft = row && !editable ? readPolicyDraft(row.idExact) : null;
  const recovered = recover ? draft : null;
  const title = id ? '编辑策略' : '新建策略';
  return (
    <InventoryPage title={title} status={state.status} error={state.status === 'error' ? state.message : undefined}
      refresh={state.status === 'error' ? refresh : undefined}
      action={<Link to={id ? policyDetailsPath(id) : '/policies'}>返回{id ? '策略详情' : '策略列表'}</Link>}>
      {data && (
        row && !editable && !recovered ? (
          <Alert type="info" showIcon message="此修订可只读查看和导出"
            action={<Flex align="center" wrap gap={8}>
              {draft && <Button onClick={() => setRecover(true)}>恢复草稿</Button>}
              <Link to={policyDetailsPath(row.idExact, row.revisionExact)}>查看策略修订</Link>
            </Flex>} />
        ) : (
          <Editor key={row?.revisionExact ?? 'new'}
            initial={editable?.values ?? recovered?.values ?? newWizardValues()}
            settings={editable?.settings ?? recovered?.settings ?? { enabled: false, mode: 'enforced' }}
            base={recovered?.base ?? (row && { id: row.idExact, revision: row.revisionExact })}
            currentRevision={row?.revisionExact} exitRecovery={recovered ? () => setRecover(false) : undefined}
            resources={data.resources} reload={reload} />
        )
      )}
    </InventoryPage>
  );
}

export default function PolicyWizard() {
  const { id } = useParams();
  return <PolicyWizardPage key={id ?? 'new'} id={id} />;
}
