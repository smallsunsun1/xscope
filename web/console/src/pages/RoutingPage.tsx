import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Alert, App, Button, Drawer, Form, Input, InputNumber, Popconfirm, Select, Space, Table, Tag, Tooltip } from "antd";
import { useState } from "react";
import { APIError, api, errorMessage } from "../api";
import { PageHeader } from "../components";
import type { Project, RoutePolicy, RoutePolicySpec } from "../types";

type Row = { project: Project; model: string; policy?: RoutePolicy };

export function RoutingPage() {
  const { message } = App.useApp();
  const client = useQueryClient();
  const projects = useQuery({ queryKey: ["projects"], queryFn: api.projects });
  const pools = useQuery({ queryKey: ["route-pools"], queryFn: api.routePools });
  const policies = useQuery({ queryKey: ["route-policies"], queryFn: api.routePolicies, refetchInterval: 10000 });
  const session = useQuery({ queryKey: ["session"], queryFn: api.session });
  const [editing, setEditing] = useState<Row>();
  const [conflict, setConflict] = useState(false);
  const [form] = Form.useForm<RoutePolicySpec>();
  const stable = Form.useWatch("stable_pool", form);
  const canary = Form.useWatch("canary_pool", form);
  const error = projects.error || pools.error || policies.error;
  const canManage = (project: Project) => session.data?.memberships?.some(m => m.tenant_id === project.tenant_id && m.role === "owner");
  const models = [...new Set((pools.data ?? []).map(pool => pool.model))];
  const rows: Row[] = (projects.data ?? []).flatMap(project => models.map(model => ({ project, model,
    policy: policies.data?.find(policy => policy.project_id === project.id && policy.model === model),
  })));
  const available = (pools.data ?? []).filter(pool => pool.model === editing?.model);
  const options = available.map(pool => ({ value: pool.id, label: `${pool.id} · ${pool.revision}` }));

  function edit(row: Row) {
    setEditing(row);
    setConflict(false);
    form.resetFields();
    form.setFieldsValue(row.policy?.spec ?? { stable_pool: (pools.data ?? []).find(pool => pool.model === row.model)?.id,
      canary_pool: null, canary_percent: 0, headers: [] });
  }

  const save = useMutation({
    mutationFn: (spec: RoutePolicySpec) => {
      if (!editing) throw new Error("请选择项目");
      return api.putRoutePolicy(editing.project.id, editing.model, editing.policy?.revision ?? 0,
        { ...spec, canary_pool: spec.canary_pool || null, headers: spec.headers ?? [] });
    },
    onSuccess: async (policy) => {
      setEditing(undefined);
      await client.invalidateQueries({ queryKey: ["route-policies"] });
      message.success(`策略 v${policy.revision} 已保存，网关通常在 5 秒内更新；已开始的请求不切池。`);
    },
    onError: async (error) => {
      setConflict(error instanceof APIError && error.status === 409);
      await client.invalidateQueries({ queryKey: ["route-policies"] });
      message.error(errorMessage(error));
    },
  });

  return <>
    <PageHeader eyebrow="TRAFFIC ROUTING" title="流量路由" description="按项目选择模型版本，先用请求头验证，再逐步调整灰度比例。" />
    <Alert type="info" showIcon title="本地模型池使用 echo Runtime，不是 GPU 模型。池由部署配置注册；新建 ModelDeployment 尚不会自动接入。" style={{ marginBottom: 20 }} />
    {error && <Alert type="error" showIcon title={errorMessage(error)} action={<Button onClick={() => { projects.refetch(); pools.refetch(); policies.refetch(); }}>重试</Button>} />}
    <section className="table-card">
      <Table<Row> rowKey={row => `${row.project.id}/${row.model}`} dataSource={rows}
        loading={projects.isLoading || pools.isLoading || policies.isLoading} pagination={{ pageSize: 10 }} scroll={{ x: 860 }}
        columns={[
          { title: "项目 / 模型", render: (_, row) => <div className="primary-cell"><span><strong>{row.project.name}</strong><small>{row.project.id} · {row.model}</small></span></div> },
          { title: "Stable 池", render: (_, row) => <code>{row.policy?.spec.stable_pool ?? "默认入口"}</code> },
          { title: "Canary 池", render: (_, row) => row.policy?.spec.canary_pool ?? "未启用" },
          { title: "权重 / 请求头", render: (_, row) => <Space orientation="vertical" size={2}><Tag color={row.policy?.spec.canary_percent ? "purple" : "default"}>Canary {row.policy?.spec.canary_percent ?? 0}%</Tag><small>{row.policy?.spec.headers.length ?? 0} 条规则优先匹配</small></Space> },
          { title: "已保存版本", render: (_, row) => row.policy ? `v${row.policy.revision}` : "未配置" },
          { title: "操作", render: (_, row) => <Tooltip title={canManage(row.project) ? "" : "仅租户 owner 可以修改"}><Button disabled={!canManage(row.project) || !!error} onClick={() => edit(row)}>配置策略</Button></Tooltip> },
        ]} />
    </section>
    <Drawer title={`流量策略 · ${editing?.project.name ?? ""}`} size={620} open={!!editing} onClose={() => !save.isPending && setEditing(undefined)} destroyOnHidden
      extra={<Popconfirm title="发布这份流量策略？" description="新请求将在快照更新后使用新策略。" onConfirm={() => form.submit()} disabled={conflict || save.isPending}>
        <Button type="primary" disabled={conflict} loading={save.isPending}>发布策略</Button>
      </Popconfirm>}>
      {conflict && <Alert type="warning" showIcon title="策略已被其他人修改。关闭并重新打开，加载最新版本后再发布。" style={{ marginBottom: 16 }} />}
      <p className="drawer-intro">{editing?.model} · 基于版本 v{editing?.policy?.revision ?? 0} 编辑。请求头规则优先，未匹配的请求按权重分流；同一请求不会自动换池重试。</p>
      <Form form={form} layout="vertical" onFinish={values => save.mutate(values)} disabled={save.isPending || conflict}>
        <Form.Item name="stable_pool" label="Stable 模型池" rules={[{ required: true }]}><Select options={options} /></Form.Item>
        <Form.Item name="canary_pool" label="Canary 模型池" dependencies={["stable_pool"]} rules={[{ validator: (_, value) => value && value === form.getFieldValue("stable_pool") ? Promise.reject(new Error("Canary 与 Stable 必须不同")) : Promise.resolve() }]}>
          <Select allowClear placeholder="不启用灰度" options={options.filter(option => option.value !== stable)} onChange={value => { if (!value) form.setFieldValue("canary_percent", 0); }} />
        </Form.Item>
        <Form.Item name="canary_percent" label="未匹配请求头时的 Canary 比例" rules={[{ required: true, type: "number", min: 0, max: 100 }]}>
          <InputNumber min={0} max={100} precision={0} suffix="%" disabled={!canary} />
        </Form.Item>
        <Form.List name="headers">
          {(fields, { add, remove }) => <>
            <h3>请求头规则</h3>
            <p>从上到下首次精确匹配生效。只允许 x-route-*，这些请求头不授予模型访问权限。</p>
            {fields.map(field => <div key={field.key} style={{ borderBottom: "1px solid #e5e7eb", marginBottom: 16 }}>
              <Form.Item name={[field.name, "name"]} label={`规则 ${field.name + 1} · 请求头名称`} rules={[{ required: true, pattern: /^x-route-[a-z0-9-]+$/, max: 64, message: "请输入小写 x-route-* 名称，最长 64 字符" }]}><Input placeholder="x-route-cohort" /></Form.Item>
              <Form.Item name={[field.name, "value"]} label="精确匹配值" rules={[{ required: true, pattern: /^[!-~]+$/, max: 256, message: "使用不含空格的 ASCII 值，最长 256 字符" }]}><Input placeholder="qa" /></Form.Item>
              <Form.Item name={[field.name, "target"]} label="目标版本" dependencies={["canary_pool"]} rules={[{ required: true }, { validator: (_, value) => value === "canary" && !form.getFieldValue("canary_pool") ? Promise.reject(new Error("请先选择 Canary 池")) : Promise.resolve() }]}>
                <Select options={[{ value: "stable", label: "Stable" }, { value: "canary", label: "Canary", disabled: !canary }]} />
              </Form.Item>
              <Button type="text" danger onClick={() => remove(field.name)}>移除规则</Button>
            </div>)}
            <Button block type="dashed" disabled={fields.length >= 16} onClick={() => add({ name: "x-route-cohort", value: "", target: "canary" })}>添加请求头规则</Button>
          </>}
        </Form.List>
        <Alert style={{ marginTop: 20 }} type="warning" showIcon title="回退到 Stable：将比例设为 0%，同时移除指向 Canary 的请求头规则，再发布。" />
      </Form>
    </Drawer>
  </>;
}
