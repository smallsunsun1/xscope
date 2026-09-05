import type { ReactNode } from "react";
import { Button, Empty, Skeleton, Tag } from "antd";
import type { LucideIcon } from "lucide-react";

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
  const options = {
    healthy: { color: "success", text: "运行正常" },
    ready: { color: "cyan", text: "已就绪" },
    pending: { color: "warning", text: "等待就绪" },
    offline: { color: "default", text: "未连接" },
  } as const;
  const option = options[status];
  return <Tag color={option.color}>{option.text}</Tag>;
}
