import { t, useI18n, useLocalizedForm } from "../i18n";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Alert, App, Button, Descriptions, Drawer, Form, Input, InputNumber, Modal, Popconfirm, Select, Table, Tag, Tooltip, Typography } from "antd";
import { Boxes, FileSearch, Gauge, Plus, RotateCcw, Trash2 } from "lucide-react";
import { useState } from "react";
import { APIError, api, errorMessage } from "../api";
import { PageHeader, ResourceEmpty, StatusTag } from "../components";
import { formatDate } from "../format";
import type { ModelDeployment } from "../types";

type DeploymentForm = {
  name: string;
  namespace: string;
  model_id: string;
  revision: string;
  uri: string;
  checksum: string;
  image: string;
  protocol: "openai" | "triton-grpc" | "custom-http";
  port: number;
  replicas: number;
  cpu_request: string;
  cpu_limit: string;
  memory_request: string;
  memory_limit: string;
  gpu: number;
  strategy: "rolling" | "canary" | "blueGreen";
};

export function DeploymentsPage() {
  useI18n();
  const { message } = App.useApp();
  const queryClient = useQueryClient();
  const [form] = Form.useForm<DeploymentForm>();
  useLocalizedForm(form);
  const [namespace, setNamespace] = useState("xscope-system");
  const [namespaceInput, setNamespaceInput] = useState("xscope-system");
  const [search, setSearch] = useState("");
  const [inspecting, setInspecting] = useState<ModelDeployment>();
  const [open, setOpen] = useState(false);
  const [scaling, setScaling] = useState<ModelDeployment>();
  const [scaleValue, setScaleValue] = useState(1);
  const deployments = useQuery({
    queryKey: ["deployments", namespace],
    queryFn: () => api.deployments(namespace),
    retry: false,
    refetchInterval: 30_000,
  });
  const models = useQuery({ queryKey: ["models"], queryFn: api.models });
  const session = useQuery({ queryKey: ["console-session"], queryFn: api.session, retry: false });
  const canManage = session.data?.memberships?.some(item => item.role === "owner") ?? false;
  const invalidate = () => queryClient.invalidateQueries({ queryKey: ["deployments", namespace] });
  const create = useMutation({
    mutationFn: (values: DeploymentForm) => {
      const gpu: Record<string, string> = values.gpu > 0 ? { "nvidia.com/gpu": String(values.gpu) } : {};
      const deployment: ModelDeployment = {
        apiVersion: "platform.xscope.io/v1alpha1",
        kind: "ModelDeployment",
        metadata: { name: values.name, namespace: values.namespace },
        spec: {
          model: { id: values.model_id, revision: values.revision, uri: values.uri, checksum: values.checksum },
          runtime: { image: values.image, protocol: values.protocol, port: values.port },
          replicas: values.replicas,
          resources: {
            requests: { cpu: values.cpu_request, memory: values.memory_request, ...gpu },
            limits: { cpu: values.cpu_limit, memory: values.memory_limit, ...gpu },
          },
          rollout: { strategy: values.strategy },
        },
      };
      return api.createDeployment(deployment);
    },
    onSuccess: async () => {
      await invalidate();
      message.success(t("模型部署已提交，Operator 将开始协调资源"));
      setOpen(false);
      form.resetFields();
    },
    onError: (error) => message.error(errorMessage(error)),
  });
  const scale = useMutation({
    mutationFn: () => {
      if (!scaling) throw new Error(t("未选择部署"));
      return api.scaleDeployment(scaling.metadata.namespace, scaling.metadata.name, scaleValue);
    },
    onSuccess: async () => {
      await invalidate();
      message.success(t("期望副本数已更新"));
      setScaling(undefined);
    },
    onError: (error) => message.error(errorMessage(error)),
  });
  const remove = useMutation({
    mutationFn: (deployment: ModelDeployment) => api.deleteDeployment(deployment.metadata.namespace, deployment.metadata.name),
    onSuccess: async () => {
      await invalidate();
      message.success(t("模型部署已删除"));
    },
    onError: (error) => message.error(errorMessage(error)),
  });

  const clusterUnavailable = deployments.error instanceof APIError && deployments.error.status === 503;
  const showCreate = () => {
    const model = models.data?.[0];
    form.setFieldsValue({
      name: model?.id ?? "",
      namespace,
      model_id: model?.id ?? "",
      revision: "v1",
      uri: model ? `s3://xscope-models/${model.id}/v1` : "",
      checksum: `sha256:${"a".repeat(64)}`,
      image: "vllm/vllm-openai:latest",
      protocol: "openai",
      port: 8000,
      replicas: 1,
      cpu_request: "2",
      cpu_limit: "4",
      memory_request: "8Gi",
      memory_limit: "16Gi",
      gpu: 1,
      strategy: "rolling",
    });
    setOpen(true);
  };
  const readyStatus = (deployment: ModelDeployment) => {
    if ((deployment.status?.readyReplicas ?? 0) >= deployment.spec.replicas && deployment.spec.replicas > 0) return "ready" as const;
    return "pending" as const;
  };

  return (
    <>
      <PageHeader
        eyebrow="Kubernetes"
        title={t("模型部署")}
        description={t("创建和管理 ModelDeployment；Operator 负责生成工作负载、Service 并回写就绪状态。")}
        action={<Button type="primary" icon={<Plus size={16} />} disabled={clusterUnavailable || !canManage} onClick={showCreate}>{t("部署模型")}</Button>}
      />
      {clusterUnavailable && (
        <Alert
          className="page-alert"
          type="info"
          showIcon
          title={t("Kubernetes 集群尚未连接")}
          description={t("控制面会继续提供项目、Key 和报价服务。配置 KUBECONFIG 或部署到集群后，本页会自动启用。")}
          action={<Button size="small" icon={<RotateCcw size={14} />} onClick={() => deployments.refetch()}>{t("重新检查")}</Button>}
        />
      )}
      {deployments.isError && !clusterUnavailable && <Alert className="page-alert" type="error" showIcon title={t("无法读取部署")} description={errorMessage(deployments.error)} />}
      <section className="panel table-panel">
        <div className="table-toolbar">
          <div className="namespace-filter"><span>Namespace</span><Input.Search aria-label={t("部署命名空间")} value={namespaceInput} onChange={(event) => setNamespaceInput(event.target.value)} onSearch={value => setNamespace(value.trim() || "xscope-system")} /></div>
          <Input aria-label={t("搜索部署")} placeholder={t("搜索部署名称 / 模型")} allowClear value={search} onChange={event => setSearch(event.target.value)} style={{ maxWidth: 230 }} />
          <Button icon={<RotateCcw size={14} />} loading={deployments.isFetching} onClick={() => deployments.refetch()}>{t("刷新")}</Button>
        </div>
        <Table<ModelDeployment>
          rowKey={(record) => `${record.metadata.namespace}/${record.metadata.name}`}
          loading={deployments.isLoading}
          dataSource={deployments.data?.filter(row => `${row.metadata.name} ${row.spec.model.id}`.toLowerCase().includes(search.toLowerCase()))}
          scroll={{ x: 900 }}
          pagination={{ pageSize: 8, hideOnSinglePage: true }}
          locale={{ emptyText: <ResourceEmpty title={deployments.isError ? t("部署数据不可用") : search ? t("没有匹配的部署") : t("此命名空间没有 ModelDeployment")} description={t("本页仅列出 Operator 管理的 CRD，不包含安装时独立创建的 Echo Runtime / Envoy / EPP。")} actionLabel={clusterUnavailable || !canManage || search ? undefined : t("部署模型")} onAction={showCreate} /> }}
          columns={[
            { title: t("部署"), render: (_, record) => <div className="primary-cell"><span className="table-icon violet"><Boxes size={16} /></span><span><strong>{record.metadata.name}</strong><small>{record.spec.model.id} · {record.spec.model.revision}</small></span></div> },
            { title: "Runtime", render: (_, record) => <div className="stack-cell"><span>{record.spec.runtime.image}</span><small>{record.spec.runtime.protocol} · :{record.spec.runtime.port}</small></div> },
            { title: t("副本"), width: 110, render: (_, record) => <div className="replica-cell"><strong>{record.status?.readyReplicas ?? 0}</strong><span>/ {record.spec.replicas}</span></div> },
            { title: t("状态"), width: 120, render: (_, record) => record.spec.replicas === 0 ? <Tag>{t("期望副本为 0")}</Tag> : <StatusTag status={readyStatus(record)} /> },
            { title: t("创建时间"), width: 150, render: (_, record) => formatDate(record.metadata.creationTimestamp) },
            {
              title: t("操作"),
              width: 120,
              align: "right",
              render: (_, record) => <div className="row-actions">
                <Tooltip title={t("查看部署详情")}><Button aria-label={t("查看 {value0} 详情", { value0: record.metadata.name })} type="text" icon={<FileSearch size={16} />} onClick={() => setInspecting(record)} /></Tooltip>
                <Tooltip title={t("调整副本")}><Button aria-label={t("调整副本")} disabled={!canManage} type="text" icon={<Gauge size={16} />} onClick={() => { setScaling(record); setScaleValue(record.spec.replicas); }} /></Tooltip>
                <Popconfirm title={t("删除模型部署？")} description={t("Operator 管理的 Deployment 和 Service 也会被回收。")} okText={t("删除")} cancelText={t("取消")} okButtonProps={{ danger: true }} onConfirm={() => remove.mutate(record)}>
                  <Tooltip title={t("删除")}><Button aria-label={t("删除部署")} disabled={!canManage} type="text" danger icon={<Trash2 size={16} />} /></Tooltip>
                </Popconfirm>
              </div>,
            },
          ]}
        />
      </section>

      <Drawer title={t("部署详情 · {value0}", { value0: inspecting?.metadata.name ?? "" })} size={600} open={Boolean(inspecting)} onClose={() => setInspecting(undefined)}>
        {inspecting && <>
          <Alert showIcon type="info" title={t("资源状态快照")} description={t("内部 Service 地址不一定能在浏览器访问。公开模型调用仍应经过 Gateway 鉴权与计费链路。")} />
          <Descriptions className="detail-descriptions" bordered column={1} items={[
            { key: "model", label: t("模型 / 版本"), children: `${inspecting.spec.model.id} / ${inspecting.spec.model.revision}` },
            { key: "cluster", label: t("集群"), children: inspecting.status?.clusterId || t("状态尚未上报") },
            { key: "namespace", label: t("命名空间"), children: inspecting.metadata.namespace },
            { key: "endpoint", label: t("内部 Endpoint"), children: inspecting.status?.endpoint ? <Typography.Text copyable>{inspecting.status.endpoint}</Typography.Text> : t("尚未分配") },
            { key: "replicas", label: t("就绪 / 期望"), children: `${inspecting.status?.readyReplicas ?? 0} / ${inspecting.spec.replicas}` },
            { key: "image", label: t("Runtime 镜像"), children: <Typography.Text copyable>{inspecting.spec.runtime.image}</Typography.Text> },
          ]} />
          <h3 className="detail-section-title">{t("协调条件")}</h3>
          {inspecting.status?.conditions?.length ? inspecting.status.conditions.map(condition => <Alert className="page-alert" key={condition.type} showIcon type={condition.status === "True" ? "info" : "warning"} title={`${condition.type}: ${condition.status} · ${condition.reason}`} description={condition.message} />) : <p className="muted">{t("Operator 尚未上报条件。")}</p>}
          <h3 className="detail-section-title">{t("资源声明（只读）")}</h3><pre className="resource-json">{JSON.stringify(inspecting.spec, null, 2)}</pre>
        </>}
      </Drawer>

      <Drawer title={t("部署模型")} size={560} open={open} onClose={() => setOpen(false)} destroyOnHidden extra={<Button type="primary" loading={create.isPending} onClick={() => form.submit()}>{t("提交部署")}</Button>}>
        <p className="drawer-intro">{t("表单会创建 `platform.xscope.io/v1alpha1` ModelDeployment。")}</p>
        <Form form={form} layout="vertical" onFinish={(values) => create.mutate(values)}>
          <div className="form-pair"><Form.Item name="name" label={t("部署名称")} rules={[{ required: true }]}><Input /></Form.Item><Form.Item name="namespace" label="Namespace" rules={[{ required: true }]}><Input /></Form.Item></div>
          <Form.Item name="model_id" label={t("模型")} rules={[{ required: true }]}><Select options={models.data?.map((model) => ({ label: `${model.display_name} · ${model.id}`, value: model.id }))} /></Form.Item>
          <div className="form-pair"><Form.Item name="revision" label={t("模型版本")} rules={[{ required: true }]}><Input /></Form.Item><Form.Item name="replicas" label={t("副本数")} rules={[{ required: true }]}><InputNumber min={0} precision={0} /></Form.Item></div>
          <Form.Item name="uri" label={t("模型 URI")} rules={[{ required: true }]}><Input placeholder="s3://bucket/model/revision" /></Form.Item>
          <Form.Item name="checksum" label={t("模型 SHA-256")} rules={[{ required: true }, { pattern: /^sha256:[a-f0-9]{64}$/, message: t("请输入 sha256: 加 64 位小写十六进制") }]}><Input /></Form.Item>
          <Form.Item name="image" label={t("Runtime 镜像")} rules={[{ required: true }]}><Input /></Form.Item>
          <div className="form-pair"><Form.Item name="protocol" label={t("协议")} rules={[{ required: true }]}><Select options={[{ value: "openai", label: "OpenAI HTTP" }, { value: "triton-grpc", label: "Triton gRPC" }, { value: "custom-http", label: "Custom HTTP" }]} /></Form.Item><Form.Item name="port" label={t("容器端口")} rules={[{ required: true }]}><InputNumber min={1} max={65535} /></Form.Item></div>
          <div className="form-section-title">{t("资源请求 / 上限")}</div>
          <div className="form-pair"><Form.Item name="cpu_request" label={t("CPU 请求")}><Input /></Form.Item><Form.Item name="cpu_limit" label={t("CPU 上限")}><Input /></Form.Item></div>
          <div className="form-pair"><Form.Item name="memory_request" label={t("内存请求")}><Input /></Form.Item><Form.Item name="memory_limit" label={t("内存上限")}><Input /></Form.Item></div>
          <div className="form-pair"><Form.Item name="gpu" label={t("GPU 数量")}><InputNumber min={0} precision={0} /></Form.Item><Form.Item name="strategy" label={t("发布策略")}><Select options={[{ value: "rolling", label: "Rolling" }, { value: "canary", label: t("Canary（下一阶段）"), disabled: true }, { value: "blueGreen", label: t("Blue / Green（下一阶段）"), disabled: true }]} /></Form.Item></div>
        </Form>
      </Drawer>

      <Modal title={t("调整副本 · {value0}", { value0: scaling?.metadata.name ?? "" })} open={Boolean(scaling)} onCancel={() => setScaling(undefined)} onOk={() => scale.mutate()} confirmLoading={scale.isPending} okText={t("更新")} cancelText={t("取消")}>
        <p className="modal-intro">{t("Operator 会将 Deployment 调整到新的期望副本数。")}</p>
        <InputNumber min={0} precision={0} value={scaleValue} onChange={(value) => setScaleValue(value ?? 0)} addonAfter={t("副本")} />
      </Modal>
    </>
  );
}
