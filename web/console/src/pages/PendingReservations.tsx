import { t, useI18n } from "../i18n";
import { useQuery } from "@tanstack/react-query";
import { Alert, Button, Descriptions, Drawer, Select, Space, Table, Tag, Typography } from "antd";
import { RefreshCw } from "lucide-react";
import { useState } from "react";
import { api, errorMessage } from "../api";
import { ResourceEmpty } from "../components";
import { formatDate, formatMicrounits } from "../format";
import type { PendingCursor, PendingReservation } from "../types";

export function PendingReservations({ projectID, currency }: { projectID: string; currency: string }) {
  useI18n();
  const [minutes, setMinutes] = useState(5);
  const [cutoff, setCutoff] = useState(() => new Date(Date.now() - 5 * 60_000).toISOString());
  const [cursors, setCursors] = useState<Array<PendingCursor | undefined>>([undefined]);
  const [selected, setSelected] = useState<PendingReservation>();
  const cursor = cursors.at(-1);
  const pending = useQuery({ queryKey: ["billing-pending", projectID, cutoff, cursor], queryFn: () => api.pendingReservations(projectID, cutoff, cursor), retry: false });
  const refresh = (age = minutes) => { setMinutes(age); setCursors([undefined]); setCutoff(new Date(Date.now() - age * 60_000).toISOString()); };
  return <>
    <div className="table-toolbar"><div><strong>{t("已派发 · 尚未结清")}</strong><div className="cell-subtitle">{t("固定时间边界分页，不对请求状态作自动裁决")}</div></div><Space wrap>
      <Select aria-label={t("待核查请求最短时长")} value={minutes} onChange={refresh} options={[5, 30, 60, 1440].map(value => ({ value, label: value === 1440 ? t("超过 24 小时") : t("超过 {value0} 分钟", { value0: value }) }))} />
      <Button icon={<RefreshCw size={14} />} loading={pending.isFetching} onClick={() => refresh()}>{t("刷新列表")}</Button>
    </Space></div>
    <Alert className="workbench-notice" showIcon type="warning" title={t("超时不等于失败，也不代表可以退款")} description={t("这里可能包含仍在推理、用量未知或用量超限的请求。必须核查 Runtime / 供应商用量证据；当前仅支持查询，不提供强制释放或零费用结算。")} />
    {pending.isError ? <Alert className="workbench-notice" type="error" showIcon title={t("无法读取待核查请求")} description={errorMessage(pending.error)} /> : <Table<PendingReservation>
      rowKey="id" loading={pending.isFetching} dataSource={pending.data?.data} pagination={false} scroll={{ x: 760 }}
      locale={{ emptyText: <ResourceEmpty title={t("当前筛选下没有待核查请求")} description={t("仅显示早于时间边界且仍为 dispatched 的记录，不代表全部请求均已结清。")} /> }}
      columns={[
        { title: t("请求"), render: (_, row) => <Button type="link" className="request-link" onClick={() => setSelected(row)}>{row.id}</Button> },
        { title: "API Key", dataIndex: "api_key_id" },
        { title: t("冻结金额"), render: (_, row) => formatMicrounits(row.reserved_microunits, currency) },
        { title: t("创建时间"), dataIndex: "created_at", render: formatDate },
        { title: t("状态"), render: () => <Tag color="warning">{t("等待用量证据")}</Tag> },
      ]} />}
    <div className="table-toolbar"><span className="muted">{t("早于 ")}{formatDate(pending.data?.created_before ?? cutoff)} {t(" · 第 ")}{cursors.length} {t(" 页")}</span><Space>
      <Button disabled={cursors.length === 1 || pending.isFetching} onClick={() => setCursors(values => values.slice(0, -1))}>{t("上一页")}</Button>
      <Button disabled={!pending.data?.next || pending.isFetching || pending.isError} onClick={() => { if (pending.data?.next) setCursors(values => [...values, pending.data!.next!]); }}>{t("下一页")}</Button>
    </Space></div>
    <Drawer title={t("待核查请求")} size={560} open={Boolean(selected)} onClose={() => setSelected(undefined)}>
      {selected && <><Alert type="info" showIcon title={t("只读查询")} description={t("按请求 ID 在日志和 Runtime 记录中查找用量证据；请求 ID 不是 trace ID。")} /><Descriptions column={1} bordered className="detail-descriptions" items={[
        { key: "id", label: t("请求 ID"), children: <Typography.Text copyable>{selected.request_id}</Typography.Text> },
        { key: "key", label: "API Key", children: selected.api_key_id },
        { key: "state", label: t("协议状态"), children: selected.state },
        { key: "amount", label: t("预占冻结"), children: formatMicrounits(selected.reserved_microunits, currency) },
        { key: "created", label: t("创建时间"), children: formatDate(selected.created_at) },
        { key: "updated", label: t("最后更新"), children: formatDate(selected.updated_at) },
      ]} /></>}
    </Drawer>
  </>;
}
