import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { App, Button, Drawer, Form, Input, Table, Tag } from "antd";
import { FolderKanban, Plus, Search } from "lucide-react";
import { useMemo, useState } from "react";
import { api, errorMessage } from "../api";
import { PageHeader, ResourceEmpty } from "../components";
import type { Project } from "../types";

export function ProjectsPage() {
  const { message } = App.useApp();
  const queryClient = useQueryClient();
  const [form] = Form.useForm<Project>();
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState("");
  const projects = useQuery({ queryKey: ["projects"], queryFn: api.projects });
  const create = useMutation({
    mutationFn: api.createProject,
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["projects"] });
      message.success("项目已创建");
      setOpen(false);
      form.resetFields();
    },
    onError: (error) => message.error(errorMessage(error)),
  });
  const filtered = useMemo(() => {
    const needle = search.trim().toLowerCase();
    if (!needle) return projects.data ?? [];
    return (projects.data ?? []).filter((item) => [item.id, item.name, item.tenant_id].some((value) => value.toLowerCase().includes(needle)));
  }, [projects.data, search]);

  const showCreate = () => {
    form.setFieldsValue({
      id: `proj-${crypto.randomUUID().slice(0, 8)}`,
      tenant_id: "tenant-local",
      name: "",
    });
    setOpen(true);
  };

  return (
    <>
      <PageHeader eyebrow="Tenancy" title="项目" description="项目是 API Key、配额、模型访问和账单归集的隔离边界。" action={<Button type="primary" icon={<Plus size={16} />} onClick={showCreate}>创建项目</Button>} />
      <section className="panel table-panel">
        <div className="table-toolbar">
          <Input allowClear prefix={<Search size={15} />} placeholder="搜索项目名称、ID 或租户" value={search} onChange={(event) => setSearch(event.target.value)} />
          <Tag icon={<FolderKanban size={13} />}>{projects.data?.length ?? 0} 个项目</Tag>
        </div>
        <Table<Project>
          rowKey="id"
          loading={projects.isLoading}
          dataSource={filtered}
          pagination={{ pageSize: 8, hideOnSinglePage: true }}
          locale={{ emptyText: <ResourceEmpty title="还没有项目" description="创建第一个项目后即可签发 API Key。" actionLabel="创建项目" onAction={showCreate} /> }}
          columns={[
            { title: "项目", dataIndex: "name", render: (name: string, record) => <div className="primary-cell"><span className="table-icon"><FolderKanban size={16} /></span><span><strong>{name}</strong><small>{record.id}</small></span></div> },
            { title: "租户", dataIndex: "tenant_id", render: (value: string) => <code className="soft-code">{value}</code> },
            { title: "状态", width: 120, render: () => <Tag color="success">正常</Tag> },
          ]}
        />
      </section>
      <Drawer title="创建项目" size={460} open={open} onClose={() => setOpen(false)} destroyOnHidden extra={<Button type="primary" loading={create.isPending} onClick={() => form.submit()}>创建</Button>}>
        <p className="drawer-intro">项目创建后，ID 和所属租户不可修改。</p>
        <Form form={form} layout="vertical" onFinish={(values) => create.mutate(values)}>
          <Form.Item name="name" label="项目名称" rules={[{ required: true, message: "请输入项目名称" }]}><Input autoFocus placeholder="例如：智能客服生产环境" /></Form.Item>
          <Form.Item name="id" label="项目 ID" rules={[{ required: true }]}><Input /></Form.Item>
          <Form.Item name="tenant_id" label="租户 ID" rules={[{ required: true }]}><Input /></Form.Item>
        </Form>
      </Drawer>
    </>
  );
}
