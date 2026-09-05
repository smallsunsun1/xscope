import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Alert, App, Button, Drawer, Form, Input, InputNumber, Modal, Popconfirm, Select, Table, Tag, Tooltip } from "antd";
import { Boxes, ExternalLink, Gauge, Plus, RotateCcw, Trash2 } from "lucide-react";
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
  const { message } = App.useApp();
  const queryClient = useQueryClient();
  const [form] = Form.useForm<DeploymentForm>();
  const [namespace, setNamespace] = useState("xscope-system");
  const [open, setOpen] = useState(false);
  const [scaling, setScaling] = useState<ModelDeployment>();
  const [scaleValue, setScaleValue] = useState(1);
  const deployments = useQuery({
    queryKey: ["deployments", namespace],
    queryFn: () => api.deployments(namespace),
    retry: false,
  });
  const models = useQuery({ queryKey: ["models"], queryFn: api.models });
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
      message.success("模型部署已提交，Operator 将开始协调资源");
      setOpen(false);
      form.resetFields();
    },
    onError: (error) => message.error(errorMessage(error)),
  });
  const scale = useMutation({
    mutationFn: () => {
      if (!scaling) throw new Error("未选择部署");
      return api.scaleDeployment(scaling.metadata.namespace, scaling.metadata.name, scaleValue);
    },
    onSuccess: async () => {
      await invalidate();
      message.success("期望副本数已更新");
      setScaling(undefined);
    },
    onError: (error) => message.error(errorMessage(error)),
  });
  const remove = useMutation({
    mutationFn: (deployment: ModelDeployment) => api.deleteDeployment(deployment.metadata.namespace, deployment.metadata.name),
    onSuccess: async () => {
      await invalidate();
      message.success("模型部署已删除");
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
        title="模型部署"
        description="创建和管理 ModelDeployment；Operator 负责生成工作负载、Service 并回写就绪状态。"
        action={<Button type="primary" icon={<Plus size={16} />} disabled={clusterUnavailable} onClick={showCreate}>部署模型</Button>}
      />
      {clusterUnavailable && (
        <Alert
          className="page-alert"
          type="info"
          showIcon
          title="Kubernetes 集群尚未连接"
          description="控制面会继续提供项目、Key 和报价服务。配置 KUBECONFIG 或部署到集群后，本页会自动启用。"
          action={<Button size="small" icon={<RotateCcw size={14} />} onClick={() => deployments.refetch()}>重新检查</Button>}
        />
      )}
      <section className="panel table-panel">
        <div className="table-toolbar">
          <div className="namespace-filter"><span>Namespace</span><Input value={namespace} onChange={(event) => setNamespace(event.target.value)} onPressEnter={() => deployments.refetch()} /></div>
          <Tag icon={<Boxes size={13} />}>{deployments.data?.length ?? 0} 个部署</Tag>
        </div>
        <Table<ModelDeployment>
          rowKey={(record) => `${record.metadata.namespace}/${record.metadata.name}`}
          loading={deployments.isLoading}
          dataSource={deployments.data}
          pagination={{ pageSize: 8, hideOnSinglePage: true }}
          locale={{ emptyText: <ResourceEmpty title={clusterUnavailable ? "等待集群连接" : "还没有模型部署"} description={clusterUnavailable ? "连接后即可查看和管理集群中的 ModelDeployment。" : "创建部署后，Operator 会自动协调推理服务。"} actionLabel={clusterUnavailable ? undefined : "部署模型"} onAction={showCreate} /> }}
          columns={[
            { title: "部署", render: (_, record) => <div className="primary-cell"><span className="table-icon violet"><Boxes size={16} /></span><span><strong>{record.metadata.name}</strong><small>{record.spec.model.id} · {record.spec.model.revision}</small></span></div> },
            { title: "Runtime", render: (_, record) => <div className="stack-cell"><span>{record.spec.runtime.image}</span><small>{record.spec.runtime.protocol} · :{record.spec.runtime.port}</small></div> },
            { title: "副本", width: 110, render: (_, record) => <div className="replica-cell"><strong>{record.status?.readyReplicas ?? 0}</strong><span>/ {record.spec.replicas}</span></div> },
            { title: "状态", width: 120, render: (_, record) => <StatusTag status={readyStatus(record)} /> },
            { title: "创建时间", width: 150, render: (_, record) => formatDate(record.metadata.creationTimestamp) },
            {
              title: "操作",
              width: 120,
              align: "right",
              render: (_, record) => <div className="row-actions">
                {record.status?.endpoint && <Tooltip title="打开 Endpoint"><Button type="text" icon={<ExternalLink size={16} />} href={`http://${record.status.endpoint}`} target="_blank" /></Tooltip>}
                <Tooltip title="调整副本"><Button type="text" icon={<Gauge size={16} />} onClick={() => { setScaling(record); setScaleValue(record.spec.replicas); }} /></Tooltip>
                <Popconfirm title="删除模型部署？" description="Operator 管理的 Deployment 和 Service 也会被回收。" okText="删除" cancelText="取消" okButtonProps={{ danger: true }} onConfirm={() => remove.mutate(record)}>
                  <Tooltip title="删除"><Button type="text" danger icon={<Trash2 size={16} />} /></Tooltip>
                </Popconfirm>
              </div>,
            },
          ]}
        />
      </section>

      <Drawer title="部署模型" size={560} open={open} onClose={() => setOpen(false)} destroyOnHidden extra={<Button type="primary" loading={create.isPending} onClick={() => form.submit()}>提交部署</Button>}>
        <p className="drawer-intro">表单会创建 `platform.xscope.io/v1alpha1` ModelDeployment。</p>
        <Form form={form} layout="vertical" onFinish={(values) => create.mutate(values)}>
          <div className="form-pair"><Form.Item name="name" label="部署名称" rules={[{ required: true }]}><Input /></Form.Item><Form.Item name="namespace" label="Namespace" rules={[{ required: true }]}><Input /></Form.Item></div>
          <Form.Item name="model_id" label="模型" rules={[{ required: true }]}><Select options={models.data?.map((model) => ({ label: `${model.display_name} · ${model.id}`, value: model.id }))} /></Form.Item>
          <div className="form-pair"><Form.Item name="revision" label="模型版本" rules={[{ required: true }]}><Input /></Form.Item><Form.Item name="replicas" label="副本数" rules={[{ required: true }]}><InputNumber min={0} precision={0} /></Form.Item></div>
          <Form.Item name="uri" label="模型 URI" rules={[{ required: true }]}><Input placeholder="s3://bucket/model/revision" /></Form.Item>
          <Form.Item name="checksum" label="模型 SHA-256" rules={[{ required: true }, { pattern: /^sha256:[a-f0-9]{64}$/, message: "请输入 sha256: 加 64 位小写十六进制" }]}><Input /></Form.Item>
          <Form.Item name="image" label="Runtime 镜像" rules={[{ required: true }]}><Input /></Form.Item>
          <div className="form-pair"><Form.Item name="protocol" label="协议" rules={[{ required: true }]}><Select options={[{ value: "openai", label: "OpenAI HTTP" }, { value: "triton-grpc", label: "Triton gRPC" }, { value: "custom-http", label: "Custom HTTP" }]} /></Form.Item><Form.Item name="port" label="容器端口" rules={[{ required: true }]}><InputNumber min={1} max={65535} /></Form.Item></div>
          <div className="form-section-title">资源请求 / 上限</div>
          <div className="form-pair"><Form.Item name="cpu_request" label="CPU 请求"><Input /></Form.Item><Form.Item name="cpu_limit" label="CPU 上限"><Input /></Form.Item></div>
          <div className="form-pair"><Form.Item name="memory_request" label="内存请求"><Input /></Form.Item><Form.Item name="memory_limit" label="内存上限"><Input /></Form.Item></div>
          <div className="form-pair"><Form.Item name="gpu" label="GPU 数量"><InputNumber min={0} precision={0} /></Form.Item><Form.Item name="strategy" label="发布策略"><Select options={[{ value: "rolling", label: "Rolling" }, { value: "canary", label: "Canary" }, { value: "blueGreen", label: "Blue / Green" }]} /></Form.Item></div>
        </Form>
      </Drawer>

      <Modal title={`调整副本 · ${scaling?.metadata.name ?? ""}`} open={Boolean(scaling)} onCancel={() => setScaling(undefined)} onOk={() => scale.mutate()} confirmLoading={scale.isPending} okText="更新" cancelText="取消">
        <p className="modal-intro">Operator 会将 Deployment 调整到新的期望副本数。</p>
        <InputNumber min={0} precision={0} value={scaleValue} onChange={(value) => setScaleValue(value ?? 0)} addonAfter="副本" />
      </Modal>
    </>
  );
}
