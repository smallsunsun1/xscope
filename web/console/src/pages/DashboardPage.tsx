import { t, useI18n } from "../i18n";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Alert, Button, Skeleton, Space, Tag } from "antd";
import { Activity, ArrowRight, Boxes, FolderKanban, KeyRound, Layers3, MessageSquareText, ReceiptText, RefreshCw, ServerCog, Sigma } from "lucide-react";
import { api, componentHealth, errorMessage } from "../api";
import { MetricCard, PageHeader, StatusTag } from "../components";
import { formatDate, formatMoney, formatNumber } from "../format";

const components = () => [
  { name: "Gateway", detail: t("Pingora · 推理入口"), component: "gateway" },
  { name: "Runtime", detail: t("推理执行服务"), component: "runtime" },
  { name: "Operator", detail: t("kube-rs · 资源协调"), component: "operator" },
  { name: "Cluster Agent", detail: t("集群适配服务"), component: "cluster-agent" },
];
function ComponentRow({ name, detail, component }: ReturnType<typeof components>[number]) {
  useI18n();
  const health = useQuery({ queryKey: ["component-health", component], queryFn: () => componentHealth(component), refetchInterval: 30_000, retry: false });
  return <div className="component-row"><div className="component-glyph"><ServerCog size={18} /></div><div className="component-copy"><strong>{name}</strong><span>{detail}</span></div>
    {health.isPending ? <Skeleton.Button active size="small" /> : <StatusTag status={health.isSuccess ? "healthy" : "offline"} />}
  </div>;
}
export function DashboardPage({ navigate }: { navigate: (page: string) => void }) {
  useI18n();
  const client = useQueryClient();
  const models = useQuery({ queryKey: ["models"], queryFn: api.models });
  const projects = useQuery({ queryKey: ["projects"], queryFn: api.projects });
  const keys = useQuery({ queryKey: ["api-keys"], queryFn: api.apiKeys });
  const billing = useQuery({ queryKey: ["billing-summary", ""], queryFn: () => api.billingSummary(), refetchInterval: 30_000, retry: false });
  const pools = useQuery({ queryKey: ["route-pools"], queryFn: api.routePools });
  const deployments = useQuery({ queryKey: ["deployments", "xscope-system"], queryFn: () => api.deployments("xscope-system"), refetchInterval: 30_000, retry: false });
  const ready = deployments.data?.reduce((sum, item) => sum + (item.status?.readyReplicas ?? 0), 0);
  const desired = deployments.data?.reduce((sum, item) => sum + item.spec.replicas, 0);
  const summary = billing.data;
  const refresh = () => Promise.all(["models", "projects", "api-keys", "billing-summary", "route-pools", "deployments", "component-health"].map(key => client.invalidateQueries({ queryKey: [key] })));
  return <>
    <PageHeader eyebrow="Platform / Overview" title={t("让每一次推理，都有迹可循。")} description={t("从运行状态到费用入账，在这里掌握平台当前的真实状态。")} action={<Space><Button icon={<RefreshCw size={15} />} onClick={refresh}>{t("刷新")}</Button><Button type="primary" onClick={() => navigate("deployments")}>{t("管理部署 ")}<ArrowRight size={15} /></Button></Space>} />
    <section className="overview-banner"><div><span className="banner-kicker">INFERENCE OPERATIONS</span><h2>{t("服务、流量与账务，一个工作台。")}</h2><p>{t("入口鉴权 → Pool 路由 → Runtime 推理 → 计量结算")}</p></div><div className="banner-aside"><span>{t("本期账务数据")}</span><strong>{summary ? `${summary.period_start.slice(0, 7)} · UTC` : t("等待数据")}</strong><small>{t("最近读取 ")}{billing.dataUpdatedAt ? formatDate(new Date(billing.dataUpdatedAt).toISOString()) : "—"}</small></div></section>
    {billing.isError && <Alert className="page-alert" showIcon type="error" title={t("用量数据暂不可用")} description={errorMessage(billing.error)} />}
    <div className="metric-grid">
      <MetricCard icon={MessageSquareText} label={t("本期已入账请求")} value={summary ? formatNumber(summary.requests) : "—"} detail={t("不包含仍在执行或待核查的请求")} loading={billing.isPending} />
      <MetricCard icon={Sigma} label={t("本期 Tokens")} value={summary ? formatNumber(summary.input_tokens + summary.output_tokens) : "—"} detail={t("来自实际用量记录，不作估算")} loading={billing.isPending} tone="blue" />
      <MetricCard icon={ReceiptText} label={t("本期费用")} value={summary ? formatMoney(summary.total.amount, summary.total.currency) : "—"} detail={t("已计费用量 · 非充值金额")} loading={billing.isPending} tone="amber" />
      <MetricCard icon={Boxes} label={t("就绪 / 期望副本")} value={deployments.data ? `${ready} / ${desired}` : "—"} detail={deployments.isError ? t("部署数据读取失败") : "ModelDeployment · xscope-system"} loading={deployments.isPending} tone="violet" />
    </div>
    <div className="inventory-strip">{[{ icon: Layers3, label: t("目录模型"), value: models.data?.length, page: "models" }, { icon: FolderKanban, label: t("项目"), value: projects.data?.length, page: "projects" }, { icon: KeyRound, label: t("有效密钥"), value: keys.data?.filter(key => key.status === "active").length, page: "keys" }, { icon: Boxes, label: t("已登记 Pool"), value: pools.data?.length, page: "routing" }].map(({ icon: Icon, label, value, page }) => <button key={page} onClick={() => navigate(page)}><Icon size={17} /><span>{label}</span><strong>{value ?? "—"}</strong><ArrowRight size={14} /></button>)}</div>
    <div className="operations-grid">
      <section className="panel"><div className="panel-heading"><div><span className="panel-kicker">Runtime health</span><h2>{t("组件存活检查")}</h2></div><Tag>{t("30 秒刷新")}</Tag></div><div className="component-list">{components().map(component => <ComponentRow key={component.component} {...component} />)}</div><p className="panel-footnote">{t("存活不等于整条推理链路就绪；调用结果请结合 trace 检查。")}</p></section>
      <section className="panel"><div className="panel-heading"><div><span className="panel-kicker">Your next action</span><h2>{t("运营工作区")}</h2></div></div><div className="workspace-actions">{[{ icon: ReceiptText, title: t("资金与待核查请求"), detail: t("检查冻结金额，定位尚未结清的请求"), page: "billing" }, { icon: Layers3, title: t("流量与灰度"), detail: t("配置 stable / canary Pool 与请求头规则"), page: "routing" }, { icon: Activity, title: t("监控与链路追踪"), detail: t("查看指标看板，按 trace ID 定位请求"), page: "observability" }].map(({ icon: Icon, title, detail, page }) => <button key={page} onClick={() => navigate(page)}><span className="workspace-action-icon"><Icon size={20} /></span><span><strong>{title}</strong><small>{detail}</small></span><ArrowRight size={17} /></button>)}</div></section>
    </div>
  </>;
}
