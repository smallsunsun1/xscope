import { t, useI18n } from "./i18n";
import { Component, type ReactNode } from "react";
import { Button, Empty, Result, Skeleton, Tag } from "antd";
import type { LucideIcon } from "lucide-react";

export class PageBoundary extends Component<{ children: ReactNode }, { failed: boolean }> {
  state = { failed: false };
  static getDerivedStateFromError() { return { failed: true }; }
  render() {
    return this.state.failed ? <Result status="warning" title={t("页面暂时无法加载")} subTitle={t("前端可能刚刚更新，或当前页面出现异常。请重新加载后再试。")} extra={<Button type="primary" onClick={() => window.location.reload()}>{t("重新加载")}</Button>} /> : this.props.children;
  }
}

export function PageHeader({
  eyebrow,
  title,
  description,
  action,
}: {
  eyebrow: string;
  title: string;
  description: string;
  action?: ReactNode;
}) {
  useI18n();
  return (
    <div className="page-header">
      <div>
        <div className="page-eyebrow">{eyebrow}</div>
        <h1>{title}</h1>
        <p>{description}</p>
      </div>
      {action && <div className="page-action">{action}</div>}
    </div>
  );
}

export function MetricCard({
  icon: Icon,
  label,
  value,
  detail,
  loading,
  tone = "mint",
}: {
  icon: LucideIcon;
  label: string;
  value: ReactNode;
  detail: string;
  loading?: boolean;
  tone?: "mint" | "blue" | "amber" | "violet";
}) {
  useI18n();
  return (
    <article className={`metric-card metric-${tone}`}>
      <div className="metric-topline">
        <span>{label}</span>
        <div className="metric-icon"><Icon size={19} /></div>
      </div>
      {loading ? <Skeleton.Input active size="small" /> : <div className="metric-value">{value}</div>}
      <div className="metric-detail">{detail}</div>
    </article>
  );
}

export function ResourceEmpty({
  title,
  description,
  actionLabel,
  onAction,
}: {
  title: string;
  description: string;
  actionLabel?: string;
  onAction?: () => void;
}) {
  useI18n();
  return (
    <div className="resource-empty">
      <Empty image={Empty.PRESENTED_IMAGE_SIMPLE} description={false} />
      <h3>{title}</h3>
      <p>{description}</p>
      {actionLabel && onAction && <Button type="primary" onClick={onAction}>{actionLabel}</Button>}
    </div>
  );
}

export function StatusTag({ status }: { status: "healthy" | "ready" | "pending" | "offline" }) {
  useI18n();
  const options = {
    healthy: { color: "success", text: t("运行正常") },
    ready: { color: "cyan", text: t("已就绪") },
    pending: { color: "warning", text: t("等待就绪") },
    offline: { color: "default", text: t("未连接") },
  } as const;
  const option = options[status];
  return <Tag color={option.color}>{option.text}</Tag>;
}
