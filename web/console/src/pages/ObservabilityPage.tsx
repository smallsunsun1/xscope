import { t, useI18n } from "../i18n";
import { Alert, App, Button, Form, Input, Space, Typography } from "antd";
import { Activity, ArrowUpRight, ChartNoAxesCombined, Search, Workflow } from "lucide-react";
import { useState } from "react";
import { PageHeader } from "../components";

const local = ["localhost", "127.0.0.1", "xscope.localhost"].includes(window.location.hostname);
function initialURL() { try { return localStorage.getItem("xscope.grafana-url") ?? (local ? "http://localhost:30083" : ""); } catch { return local ? "http://localhost:30083" : ""; } }
function grafanaBase(value: string) {
  const url = new URL(value);
  if (!["http:", "https:"].includes(url.protocol) || url.username || url.password || url.search || url.hash) throw new Error(t("请输入不含凭证、查询参数或片段的 HTTP(S) Grafana 地址"));
  return url.href.replace(/\/$/, "");
}
export function ObservabilityPage() {
  useI18n();
  const { message } = App.useApp();
  const [address, setAddress] = useState(initialURL);
  const [trace, setTrace] = useState("");
  const open = (path: string) => { try { window.open(`${grafanaBase(address)}${path}`, "_blank", "noopener,noreferrer"); } catch (error) { message.error(error instanceof Error ? error.message : t("地址无效")); } };
  const traceValid = /^[a-f0-9]{32}$/i.test(trace.trim()) && !/^0+$/.test(trace.trim());
  const search = () => {
    if (!traceValid) return;
    const panes = { trace: { datasource: "xscope-jaeger", queries: [{ refId: "A", query: trace.trim().toLowerCase(), datasource: { type: "jaeger", uid: "xscope-jaeger" } }], range: { from: "now-1h", to: "now" } } };
    open(`/explore?${new URLSearchParams({ schemaVersion: "1", panes: JSON.stringify(panes) })}`);
  };
  return <>
    <PageHeader eyebrow="Observe / Diagnose" title={t("从指标，到每一次请求。")} description={t("复用现有 Grafana 和 Jaeger，控制台不复制监控数据，也不额外启动采集服务。")} />
    <section className="overview-banner"><div><span className="banner-kicker">METRICS + TRACES</span><h2>{t("先发现异常，再还原现场。")}</h2><p>{t("趋势看板发现变化 · 请求列表筛选慢调用 · Trace 查看跨服务耗时")}</p></div><Activity size={60} strokeWidth={1} aria-hidden="true" /></section>
    <div className="observe-grid">{[
      { icon: ChartNoAxesCombined, title: t("平台指标"), detail: t("请求量、延迟、用量投递和组件状态。"), path: "/d/xscope-overview/xscope-overview?from=now-1h&to=now", label: t("打开指标看板") },
      { icon: Workflow, title: t("请求追踪"), detail: t("按服务与耗时筛选，再展开一次请求的调用链。"), path: "/d/xscope-traces/xscope-traces?from=now-1h&to=now", label: t("打开追踪看板") },
      { icon: Activity, title: t("自由查询"), detail: t("通过 Grafana Explore 查询 Prometheus 或 Jaeger。"), path: "/explore", label: t("进入 Explore") },
    ].map(({ icon: Icon, title, detail, path, label }) => <section className="panel observe-card" key={title}><span className="workspace-action-icon"><Icon size={23} /></span><h2>{title}</h2><p>{detail}</p><Button onClick={() => open(path)}>{label}<ArrowUpRight size={15} /></Button></section>)}</div>
    <section className="panel trace-search"><span className="panel-kicker">Find a request</span><h2>{t("已有 Trace ID？直接定位。")}</h2><p>{t("使用推理响应头 ")}<Typography.Text code>x-trace-id</Typography.Text>{t("，不是 billing request ID。")}</p><Space.Compact block><Input aria-label="Trace ID" placeholder={t("32 位十六进制 Trace ID")} value={trace} onChange={event => setTrace(event.target.value)} onPressEnter={search} status={trace && !traceValid ? "error" : undefined} /><Button type="primary" icon={<Search size={15} />} disabled={!traceValid} onClick={search}>{t("查看链路")}</Button></Space.Compact><p className="muted">{t("过期、未采样或尚未导出的请求可能查不到；请结合日志核查。")}</p></section>
    <section className="panel trace-search"><h2>{t("监控入口地址")}</h2><p>{t("本地环境默认使用 NodePort 30083。远程环境请填写可从当前浏览器访问的 Grafana 地址。")}</p><Form onFinish={() => { try { const url = grafanaBase(address); localStorage.setItem("xscope.grafana-url", url); setAddress(url); message.success(t("地址已保存到当前浏览器")); } catch (error) { message.error(error instanceof Error ? error.message : t("无法保存")); } }}><Space.Compact block><Input aria-label={t("Grafana 地址")} value={address} placeholder="https://grafana.example.com" onChange={event => setAddress(event.target.value)} /><Button htmlType="submit">{t("保存地址")}</Button></Space.Compact></Form></section>
    <Alert type="info" showIcon title={t("访问说明")} description={t("这些入口在新标签页打开，不代表服务健康检查结果。Grafana 使用自己的登录会话；这里不会保存或传递账号密码。")} />
  </>;
}
