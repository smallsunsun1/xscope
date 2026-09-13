import { t, useI18n, useLocalizedForm } from "../i18n";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Alert, App, Button, Drawer, Form, Input, InputNumber, Popconfirm, Select, Space, Table, Tag, Tooltip } from "antd";
import { useState } from "react";
import { APIError, api, errorMessage } from "../api";
import { PageHeader } from "../components";
import type { Project, RoutePolicy, RoutePolicySpec } from "../types";
import { ReleaseHistory } from "./ReleaseHistory";

type Row = { project: Project; model: string; policy?: RoutePolicy };

export function RoutingPage() {
  useI18n();
  const { message } = App.useApp();
  const client = useQueryClient();
  const projects = useQuery({ queryKey: ["projects"], queryFn: api.projects });
  const pools = useQuery({ queryKey: ["route-pools"], queryFn: api.routePools });
  const policies = useQuery({ queryKey: ["route-policies"], queryFn: api.routePolicies, refetchInterval: 10000 });
  const session = useQuery({ queryKey: ["session"], queryFn: api.session, retry: false });
  const [editing, setEditing] = useState<Row>();
  const [history, setHistory] = useState<Row>();
  const [conflict, setConflict] = useState(false);
  const [form] = Form.useForm<RoutePolicySpec>();
  useLocalizedForm(form);
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
      if (!editing) throw new Error(t("请选择项目"));
      return api.putRoutePolicy(editing.project.id, editing.model, editing.policy?.revision ?? 0,
        { ...spec, canary_pool: spec.canary_pool || null, headers: spec.headers ?? [] });
    },
    onSuccess: async (policy) => {
      setEditing(undefined);
      await client.invalidateQueries({ queryKey: ["route-policies"] });
      message.success(t("策略 v{value0} 已保存，网关通常在 5 秒内更新；已开始的请求不切池。", { value0: policy.revision }));
    },
    onError: async (error) => {
      setConflict(error instanceof APIError && error.status === 409);
      await client.invalidateQueries({ queryKey: ["route-policies"] });
      message.error(errorMessage(error));
    },
  });

  return <>
    <PageHeader eyebrow="TRAFFIC ROUTING" title={t("流量路由")} description={t("按项目选择模型版本，先用请求头验证，再逐步调整灰度比例。")} />
    <Alert type="info" showIcon title={t("新建 ModelDeployment 需绑定受管入口才会自动注册模型池；本地仍使用 Echo。")} style={{ marginBottom: 20 }} />
    {error && <Alert type="error" showIcon title={errorMessage(error)} action={<Button onClick={() => { projects.refetch(); pools.refetch(); policies.refetch(); }}>{t("重试")}</Button>} />}
    <section className="table-card">
      <Table<Row> rowKey={row => `${row.project.id}/${row.model}`} dataSource={rows}
        loading={projects.isLoading || pools.isLoading || policies.isLoading} pagination={{ pageSize: 10 }} scroll={{ x: 860 }}
        columns={[
          { title: t("项目 / 模型"), render: (_, row) => <div className="primary-cell"><span><strong>{row.project.name}</strong><small>{row.project.id} · {row.model}</small></span></div> },
          { title: t("Stable 池"), render: (_, row) => <code>{row.policy?.spec.stable_pool ?? t("默认入口")}</code> },
          { title: t("Canary 池"), render: (_, row) => row.policy?.spec.canary_pool ?? t("未启用") },
          { title: t("权重 / 请求头"), render: (_, row) => <Space orientation="vertical" size={2}><Tag color={row.policy?.spec.canary_percent ? "purple" : "default"}>Canary {row.policy?.spec.canary_percent ?? 0}%</Tag><small>{row.policy?.spec.headers.length ?? 0} {t(" 条规则优先匹配")}</small></Space> },
          { title: t("已保存版本"), render: (_, row) => row.policy ? `v${row.policy.revision}` : t("未配置") },
          { title: t("操作"), render: (_, row) => <Space><Tooltip title={canManage(row.project) ? "" : t("仅租户 owner 可以修改")}><Button disabled={!canManage(row.project) || !!error} onClick={() => edit(row)}>{t("配置策略")}</Button></Tooltip><Button disabled={!row.policy} onClick={() => setHistory(row)}>{t("发布历史")}</Button></Space> },
        ]} />
    </section>
    <Drawer title={t("发布历史")} size={780} open={!!history} onClose={() => setHistory(undefined)} destroyOnHidden>{history?.policy && <ReleaseHistory key={`${history.project.id}/${history.model}`} policy={history.policy} canManage={!!canManage(history.project)} />}</Drawer>
    <Drawer title={t("流量策略 · {value0}", { value0: editing?.project.name ?? "" })} size={620} open={!!editing} onClose={() => !save.isPending && setEditing(undefined)} destroyOnHidden
      extra={<Popconfirm title={t("发布这份流量策略？")} description={t("新请求将在快照更新后使用新策略。")} onConfirm={() => form.submit()} disabled={conflict || save.isPending}>
        <Button type="primary" disabled={conflict} loading={save.isPending}>{t("发布流量策略")}</Button>
      </Popconfirm>}>
      {conflict && <Alert type="warning" showIcon title={t("策略已被其他人修改。关闭并重新打开，加载最新版本后再发布。")} style={{ marginBottom: 16 }} />}
      <p className="drawer-intro">{editing?.model} {t(" · 基于版本 v")}{editing?.policy?.revision ?? 0} {t(" 编辑。请求头规则优先，未匹配的请求按权重分流；同一请求不会自动换池重试。")}</p>
      <Form form={form} layout="vertical" onFinish={values => save.mutate(values)} disabled={save.isPending || conflict}>
        <Form.Item name="stable_pool" label={t("Stable 模型池")} rules={[{ required: true }]}><Select options={options} /></Form.Item>
        <Form.Item name="canary_pool" label={t("Canary 模型池")} dependencies={["stable_pool"]} rules={[{ validator: (_, value) => value && value === form.getFieldValue("stable_pool") ? Promise.reject(new Error(t("Canary 与 Stable 必须不同"))) : Promise.resolve() }]}>
          <Select allowClear placeholder={t("不启用灰度")} options={options.filter(option => option.value !== stable)} onChange={value => { if (!value) form.setFieldValue("canary_percent", 0); }} />
        </Form.Item>
        <Form.Item name="canary_percent" label={t("未匹配请求头时的 Canary 比例")} rules={[{ required: true, type: "number", min: 0, max: 100 }]}>
          <InputNumber min={0} max={100} precision={0} suffix="%" disabled={!canary} />
        </Form.Item>
        <Form.List name="headers">
          {(fields, { add, remove }) => <>
            <h3>{t("请求头规则")}</h3>
            <p>{t("从上到下首次精确匹配生效。只允许 x-route-*，这些请求头不授予模型访问权限。")}</p>
            {fields.map(field => <div key={field.key} style={{ borderBottom: "1px solid #e5e7eb", marginBottom: 16 }}>
              <Form.Item name={[field.name, "name"]} label={t("规则 {value0} · 请求头名称", { value0: field.name + 1 })} rules={[{ required: true, pattern: /^x-route-[a-z0-9-]+$/, max: 64, message: t("请输入小写 x-route-* 名称，最长 64 字符") }]}><Input placeholder="x-route-cohort" /></Form.Item>
              <Form.Item name={[field.name, "value"]} label={t("精确匹配值")} rules={[{ required: true, pattern: /^[!-~]+$/, max: 256, message: t("使用不含空格的 ASCII 值，最长 256 字符") }]}><Input placeholder="qa" /></Form.Item>
              <Form.Item name={[field.name, "target"]} label={t("目标版本")} dependencies={["canary_pool"]} rules={[{ required: true }, { validator: (_, value) => value === "canary" && !form.getFieldValue("canary_pool") ? Promise.reject(new Error(t("请先选择 Canary 池"))) : Promise.resolve() }]}>
                <Select options={[{ value: "stable", label: "Stable" }, { value: "canary", label: "Canary", disabled: !canary }]} />
              </Form.Item>
              <Button type="text" danger onClick={() => remove(field.name)}>{t("移除规则")}</Button>
            </div>)}
            <Button block type="dashed" disabled={fields.length >= 16} onClick={() => add({ name: "x-route-cohort", value: "", target: "canary" })}>{t("添加请求头规则")}</Button>
          </>}
        </Form.List>
        <Alert style={{ marginTop: 20 }} type="warning" showIcon title={t("回退到 Stable：将比例设为 0%，同时移除指向 Canary 的请求头规则，再发布。")} />
      </Form>
    </Drawer>
  </>;
}
