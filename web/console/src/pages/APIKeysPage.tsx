import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { App, Button, Drawer, Form, Input, Modal, Popconfirm, Select, Table, Tag, Tooltip } from "antd";
import { Check, Copy, KeyRound, Plus, ShieldCheck, Trash2 } from "lucide-react";
import { useState } from "react";
import { api, errorMessage } from "../api";
import { PageHeader, ResourceEmpty } from "../components";
import { formatDate } from "../format";
import type { APIKey, IssuedAPIKey } from "../types";

type KeyForm = {
  id: string;
  project_id: string;
  name: string;
};

export function APIKeysPage() {
  const { message } = App.useApp();
  const queryClient = useQueryClient();
  const [form] = Form.useForm<KeyForm>();
  const [open, setOpen] = useState(false);
  const [issued, setIssued] = useState<IssuedAPIKey>();
  const [copied, setCopied] = useState(false);
  const projects = useQuery({ queryKey: ["projects"], queryFn: api.projects });
  const keys = useQuery({ queryKey: ["api-keys"], queryFn: api.apiKeys });
  const create = useMutation({
    mutationFn: async (values: KeyForm) => {
      const project = projects.data?.find((item) => item.id === values.project_id);
      if (!project) throw new Error("请选择有效项目");
      return api.createAPIKey({ ...values, tenant_id: project.tenant_id });
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
      message.success("API Key 已撤销");
    },
    onError: (error) => message.error(errorMessage(error)),
  });

  const showCreate = () => {
    form.setFieldsValue({ id: `key-${crypto.randomUUID().slice(0, 8)}`, name: "" });
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
      <PageHeader eyebrow="Access" title="API Keys" description="为项目签发访问凭证。明文密钥只会在创建后展示一次。" action={<Button type="primary" icon={<Plus size={16} />} disabled={!projects.data?.length} onClick={showCreate}>签发 Key</Button>} />
      {!projects.isLoading && !projects.data?.length && <div className="inline-notice"><ShieldCheck size={18} /><span>请先创建项目，再为项目签发 API Key。</span></div>}
      <section className="panel table-panel">
        <Table<APIKey>
          rowKey="id"
          loading={keys.isLoading}
          dataSource={keys.data}
          pagination={{ pageSize: 8, hideOnSinglePage: true }}
          locale={{ emptyText: <ResourceEmpty title="没有有效的 API Key" description="签发后，Key 可用于调用 Pingora 网关。" actionLabel={projects.data?.length ? "签发 Key" : undefined} onAction={showCreate} /> }}
          columns={[
            { title: "名称", dataIndex: "name", render: (name: string, record) => <div className="primary-cell"><span className="table-icon amber"><KeyRound size={16} /></span><span><strong>{name}</strong><small>{record.id}</small></span></div> },
            { title: "项目", dataIndex: "project_id", render: (value: string) => <code className="soft-code">{value}</code> },
            { title: "创建时间", dataIndex: "created_at", render: formatDate },
            { title: "状态", width: 100, render: () => <Tag color="success">有效</Tag> },
            {
              title: "操作",
              width: 80,
              align: "right",
              render: (_, record) => (
                <Popconfirm title="撤销 API Key？" description="撤销后，使用该 Key 的请求会立即失败。" okText="撤销" cancelText="取消" okButtonProps={{ danger: true }} onConfirm={() => revoke.mutate(record.id)}>
                  <Tooltip title="撤销"><Button danger type="text" icon={<Trash2 size={16} />} /></Tooltip>
                </Popconfirm>
              ),
            },
          ]}
        />
      </section>
      <Drawer title="签发 API Key" size={460} open={open} onClose={() => setOpen(false)} destroyOnHidden extra={<Button type="primary" loading={create.isPending} onClick={() => form.submit()}>签发</Button>}>
        <p className="drawer-intro">Key 将归属于所选项目和对应租户。</p>
        <Form form={form} layout="vertical" onFinish={(values) => create.mutate(values)}>
          <Form.Item name="name" label="Key 名称" rules={[{ required: true, message: "请输入名称" }]}><Input autoFocus placeholder="例如：production-gateway" /></Form.Item>
          <Form.Item name="project_id" label="所属项目" rules={[{ required: true, message: "请选择项目" }]}>
            <Select placeholder="选择项目" options={projects.data?.map((project) => ({ label: `${project.name} · ${project.id}`, value: project.id }))} />
          </Form.Item>
          <Form.Item name="id" label="Key ID" rules={[{ required: true }]}><Input /></Form.Item>
        </Form>
      </Drawer>
      <Modal open={Boolean(issued)} title="API Key 已签发" footer={<Button type="primary" onClick={() => setIssued(undefined)}>我已保存</Button>} closable={false} maskClosable={false}>
        <div className="secret-success"><span><Check size={26} /></span><div><strong>请立即复制并安全保存</strong><p>关闭此窗口后，控制台不会再次展示明文密钥。</p></div></div>
        <div className="secret-box"><code>{issued?.secret}</code><Button icon={copied ? <Check size={16} /> : <Copy size={16} />} onClick={copySecret}>{copied ? "已复制" : "复制"}</Button></div>
      </Modal>
    </>
  );
}
