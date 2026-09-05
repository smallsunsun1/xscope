import { t, useI18n, useLocalizedForm } from "../i18n";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { App, Button, Drawer, Form, Input, InputNumber, Modal, Popconfirm, Select, Table, Tag, Tooltip } from "antd";
import { Check, Copy, KeyRound, Plus, ShieldCheck, Trash2 } from "lucide-react";
import { useState } from "react";
import { api, errorMessage } from "../api";
import { PageHeader, ResourceEmpty } from "../components";
import { formatDate, formatMoney, formatNumber } from "../format";
import type { APIKey, IssuedAPIKey } from "../types";

type KeyForm = {
  id: string;
  project_id: string;
  name: string;
  scopes: string[];
  allowed_models: string[];
  expires_in_days: number;
  rate_limit_rpm: number;
  rate_limit_tpm: number;
  monthly_budget_yuan: number;
};

const statusTag = () => ({
  active: <Tag color="success">{t("有效")}</Tag>,
  expired: <Tag color="warning">{t("已过期")}</Tag>,
  revoked: <Tag>{t("已撤销")}</Tag>,
});

export function APIKeysPage() {
  useI18n();
  const { message } = App.useApp();
  const queryClient = useQueryClient();
  const [form] = Form.useForm<KeyForm>();
  useLocalizedForm(form);
  const [open, setOpen] = useState(false);
  const [issued, setIssued] = useState<IssuedAPIKey>();
  const [copied, setCopied] = useState(false);
  const projects = useQuery({ queryKey: ["projects"], queryFn: api.projects });
  const models = useQuery({ queryKey: ["models"], queryFn: api.models });
  const keys = useQuery({ queryKey: ["api-keys"], queryFn: api.apiKeys });
  const create = useMutation({
    mutationFn: async (values: KeyForm) => {
      const project = projects.data?.find((item) => item.id === values.project_id);
      if (!project) throw new Error(t("请选择有效项目"));
      const expiresAt = values.expires_in_days
        ? new Date(Date.now() + values.expires_in_days * 86_400_000).toISOString()
        : undefined;
      return api.createAPIKey({
        id: values.id,
        name: values.name,
        project_id: values.project_id,
        tenant_id: project.tenant_id,
        scopes: values.scopes,
        allowed_models: values.allowed_models,
        expires_at: expiresAt,
        rate_limit_rpm: values.rate_limit_rpm,
        rate_limit_tpm: values.rate_limit_tpm,
        monthly_budget: { currency: "CNY", amount: Math.round(values.monthly_budget_yuan * 100) },
      });
    },
    onSuccess: async (result) => {
      await queryClient.invalidateQueries({ queryKey: ["api-keys"] });
      setOpen(false);
      setIssued(result);
      form.resetFields();
    },
    onError: (error) => message.error(errorMessage(error)),
  });
  const revoke = useMutation({
    mutationFn: api.revokeAPIKey,
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["api-keys"] });
      message.success(t("API Key 已撤销；网关将在 5 秒内停止接受它"));
    },
    onError: (error) => message.error(errorMessage(error)),
  });

  const showCreate = () => {
    form.setFieldsValue({
      id: `key-${crypto.randomUUID().slice(0, 8)}`,
      name: "",
      scopes: ["chat.completions"],
      allowed_models: models.data?.map((model) => model.id) ?? [],
      expires_in_days: 90,
      rate_limit_rpm: 60,
      rate_limit_tpm: 60_000,
      monthly_budget_yuan: 100,
    });
    setOpen(true);
  };
  const copySecret = async () => {
    if (!issued) return;
    await navigator.clipboard.writeText(issued.secret);
    setCopied(true);
    window.setTimeout(() => setCopied(false), 1800);
  };

  return (
    <>
      <PageHeader eyebrow="Access" title="API Keys" description={t("按项目限制权限、模型、有效期、请求速率与月度预算；网关动态消费哈希策略快照。")} action={<Button type="primary" icon={<Plus size={16} />} disabled={!projects.data?.length} onClick={showCreate}>{t("签发 Key")}</Button>} />
      {!projects.isLoading && !projects.data?.length && <div className="inline-notice"><ShieldCheck size={18} /><span>{t("请先创建项目，再为项目签发 API Key。")}</span></div>}
      <section className="panel table-panel">
        <Table<APIKey>
          rowKey="id"
          loading={keys.isLoading}
          dataSource={keys.data}
          scroll={{ x: 1080 }}
          pagination={{ pageSize: 8, hideOnSinglePage: true }}
          locale={{ emptyText: <ResourceEmpty title={t("没有 API Key")} description={t("签发后，Key 可用于调用 Pingora 网关。")} actionLabel={projects.data?.length ? t("签发 Key") : undefined} onAction={showCreate} /> }}
          columns={[
            { title: t("名称"), dataIndex: "name", fixed: "left", width: 190, render: (name: string, record) => <div className="primary-cell"><span className="table-icon amber"><KeyRound size={16} /></span><span><strong>{name}</strong><small>{record.id}</small></span></div> },
            { title: t("项目"), dataIndex: "project_id", width: 150, render: (value: string) => <code className="soft-code">{value}</code> },
            { title: t("允许模型"), dataIndex: "allowed_models", width: 180, render: (values: string[]) => values.map((value) => <Tag key={value}>{value}</Tag>) },
            { title: t("速率"), width: 150, render: (_, record) => <><div>{record.rate_limit_rpm} RPM</div><small>{formatNumber(record.rate_limit_tpm)} TPM</small></> },
            { title: t("月度预算"), dataIndex: "monthly_budget", width: 120, render: (value: APIKey["monthly_budget"]) => value.amount ? formatMoney(value.amount, value.currency) : t("不限") },
            { title: t("到期时间"), dataIndex: "expires_at", width: 170, render: (value?: string) => value ? formatDate(value) : t("永不过期") },
            { title: t("状态"), dataIndex: "status", width: 100, render: (value: APIKey["status"]) => statusTag()[value] },
            {
              title: t("操作"), width: 80, fixed: "right", align: "right",
              render: (_, record) => record.status === "active" ? (
                <Popconfirm title={t("撤销 API Key？")} description={t("策略同步后，使用该 Key 的请求会立即失败。")} okText={t("撤销")} cancelText={t("取消")} okButtonProps={{ danger: true }} onConfirm={() => revoke.mutate(record.id)}>
                  <Tooltip title={t("撤销")}><Button danger type="text" icon={<Trash2 size={16} />} /></Tooltip>
                </Popconfirm>
              ) : null,
            },
          ]}
        />
      </section>
      <Drawer title={t("签发 API Key")} size={520} open={open} onClose={() => setOpen(false)} destroyOnHidden extra={<Button type="primary" loading={create.isPending} onClick={() => form.submit()}>{t("签发")}</Button>}>
        <p className="drawer-intro">{t("策略在控制面持久化，网关仅消费不含明文密钥的哈希快照。")}</p>
        <Form form={form} layout="vertical" onFinish={(values) => create.mutate(values)}>
          <Form.Item name="name" label={t("Key 名称")} rules={[{ required: true, message: t("请输入名称") }]}><Input autoFocus placeholder={t("例如：production-gateway")} /></Form.Item>
          <Form.Item name="project_id" label={t("所属项目")} rules={[{ required: true, message: t("请选择项目") }]}>
            <Select placeholder={t("选择项目")} options={projects.data?.map((project) => ({ label: `${project.name} · ${project.id}`, value: project.id }))} />
          </Form.Item>
          <Form.Item name="allowed_models" label={t("允许模型")} rules={[{ required: true, message: t("至少允许一个模型") }]}>
            <Select mode="multiple" options={models.data?.map((model) => ({ label: model.display_name, value: model.id }))} />
          </Form.Item>
          <Form.Item name="scopes" label={t("权限 Scope")} rules={[{ required: true }]}>
            <Select mode="multiple" options={[{ label: "Chat Completions", value: "chat.completions" }]} />
          </Form.Item>
          <div className="form-pair">
            <Form.Item name="rate_limit_rpm" label={t("每分钟请求数")} rules={[{ required: true }]}><InputNumber min={1} max={1_000_000} precision={0} /></Form.Item>
            <Form.Item name="rate_limit_tpm" label={t("每分钟 Token 数")} rules={[{ required: true }]}><InputNumber min={1} max={10_000_000_000} precision={0} /></Form.Item>
          </div>
          <div className="form-pair">
            <Form.Item name="expires_in_days" label={t("有效天数（0 为永久）")} rules={[{ required: true }]}><InputNumber min={0} max={3650} precision={0} /></Form.Item>
          </div>
          <Form.Item name="monthly_budget_yuan" label={t("月度预算（人民币，0 为不限）")} rules={[{ required: true }]}><InputNumber min={0} precision={2} prefix="¥" /></Form.Item>
          <Form.Item name="id" label="Key ID" rules={[{ required: true }]}><Input /></Form.Item>
        </Form>
      </Drawer>
      <Modal open={Boolean(issued)} title={t("API Key 已签发")} footer={<Button type="primary" onClick={() => setIssued(undefined)}>{t("我已保存")}</Button>} closable={false} maskClosable={false}>
        <div className="secret-success"><span><Check size={26} /></span><div><strong>{t("请立即复制并安全保存")}</strong><p>{t("数据库和控制台只保存 SHA-256 摘要，关闭后无法再次查看明文。")}</p></div></div>
        <div className="secret-box"><code>{issued?.secret}</code><Button icon={copied ? <Check size={16} /> : <Copy size={16} />} onClick={copySecret}>{copied ? t("已复制") : t("复制")}</Button></div>
      </Modal>
    </>
  );
}
