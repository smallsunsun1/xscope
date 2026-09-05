import { useQuery } from "@tanstack/react-query";
import { Alert, Button, Progress, Skeleton } from "antd";
import {
  ArrowRight,
  Boxes,
  FolderKanban,
  KeyRound,
  Layers3,
  ReceiptText,
  ServerCog,
  Sparkles,
} from "lucide-react";
import { api, componentHealth } from "../api";
import { MetricCard, PageHeader, StatusTag } from "../components";

type Navigate = (page: string) => void;

const components = [
  { name: "Gateway", detail: "Pingora · :8080", component: "gateway" },
  { name: "Runtime", detail: "FastAPI · :8090", component: "runtime" },
  { name: "Operator", detail: "controller-runtime · :8082", component: "operator" },
];

function ComponentRow({ name, detail, component }: (typeof components)[number]) {
  const health = useQuery({
    queryKey: ["component-health", component],
    queryFn: () => componentHealth(component),
    refetchInterval: 15_000,
    retry: false,
  });
  return (
    <div className="component-row">
      <div className="component-glyph"><ServerCog size={18} /></div>
      <div className="component-copy">
        <strong>{name}</strong>
        <span>{detail}</span>
      </div>
      {health.isLoading ? <Skeleton.Button active size="small" /> : <StatusTag status={health.isSuccess ? "healthy" : "offline"} />}
    </div>
  );
}

export function DashboardPage({ navigate }: { navigate: Navigate }) {
  const models = useQuery({ queryKey: ["models"], queryFn: api.models });
  const projects = useQuery({ queryKey: ["projects"], queryFn: api.projects });
  const keys = useQuery({ queryKey: ["api-keys"], queryFn: api.apiKeys });
  const deployments = useQuery({
    queryKey: ["deployments", "xscope-system"],
    queryFn: () => api.deployments("xscope-system"),
    retry: false,
  });
  const readyReplicas = deployments.data?.reduce((sum, item) => sum + (item.status?.readyReplicas ?? 0), 0) ?? 0;
  const desiredReplicas = deployments.data?.reduce((sum, item) => sum + item.spec.replicas, 0) ?? 0;
  const readiness = desiredReplicas ? Math.round((readyReplicas / desiredReplicas) * 100) : 0;

  return (
    <>
      <PageHeader
        eyebrow="Overview"
        title="平台总览"
        description="从一个视图检查服务状态、资源规模和下一步操作。"
        action={<Button type="primary" icon={<Sparkles size={16} />} onClick={() => navigate("deployments")}>部署模型</Button>}
      />

      <div className="metric-grid">
        <MetricCard icon={Layers3} label="可用模型" value={models.data?.length ?? 0} detail="来自模型目录" loading={models.isLoading} />
        <MetricCard icon={FolderKanban} label="项目" value={projects.data?.length ?? 0} detail="隔离租户资源" loading={projects.isLoading} tone="blue" />
        <MetricCard icon={KeyRound} label="有效 API Key" value={keys.data?.length ?? 0} detail="仅展示元数据" loading={keys.isLoading} tone="amber" />
        <MetricCard
          icon={Boxes}
          label="模型部署"
          value={deployments.isError ? "—" : (deployments.data?.length ?? 0)}
          detail={deployments.isError ? "集群尚未连接" : `${readyReplicas}/${desiredReplicas} 副本就绪`}
          loading={deployments.isLoading}
          tone="violet"
        />
      </div>

      <div className="dashboard-grid">
        <section className="panel service-panel">
          <div className="panel-heading">
            <div><span className="panel-kicker">Live checks</span><h2>组件状态</h2></div>
            <span className="refresh-note">每 15 秒刷新</span>
          </div>
          <div className="component-list">
            {components.map((component) => <ComponentRow key={component.name} {...component} />)}
          </div>
        </section>

        <section className="panel readiness-panel">
          <div className="panel-heading">
            <div><span className="panel-kicker">Kubernetes</span><h2>部署就绪度</h2></div>
          </div>
          {deployments.isError ? (
            <Alert type="info" showIcon title="集群未连接" description="配置 kubeconfig 或在集群内运行控制面后，即可在这里管理 ModelDeployment。" />
          ) : (
            <div className="readiness-content">
              <Progress type="dashboard" percent={readiness} strokeColor="#20a981" railColor="#e7efec" />
              <div><strong>{readyReplicas} 个副本已就绪</strong><span>期望副本 {desiredReplicas} · 部署 {deployments.data?.length ?? 0}</span></div>
            </div>
          )}
        </section>
      </div>

      <section className="panel quick-panel">
        <div className="panel-heading">
          <div><span className="panel-kicker">Get started</span><h2>常用操作</h2></div>
        </div>
        <div className="quick-grid">
          <button className="quick-action" onClick={() => navigate("projects")}><FolderKanban /><span><strong>创建项目</strong><small>建立租户资源边界</small></span><ArrowRight /></button>
          <button className="quick-action" onClick={() => navigate("keys")}><KeyRound /><span><strong>签发 API Key</strong><small>为项目创建访问凭证</small></span><ArrowRight /></button>
          <button className="quick-action" onClick={() => navigate("models")}><Layers3 /><span><strong>估算调用费用</strong><small>按模型与 token 数报价</small></span><ArrowRight /></button>
          <button className="quick-action" onClick={() => navigate("billing")}><ReceiptText /><span><strong>查看用量账单</strong><small>按项目聚合请求与费用</small></span><ArrowRight /></button>
        </div>
      </section>
    </>
  );
}
