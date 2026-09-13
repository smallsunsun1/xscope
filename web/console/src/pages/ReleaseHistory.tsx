import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Alert, App, Button, Popconfirm, Space, Table, Tag } from "antd";
import { APIError, errorMessage } from "../api";
import { formatDate } from "../format";
import { t } from "../i18n";
import { ops } from "../operations";
import type { RoutePolicy } from "../types";

export function ReleaseHistory({ policy: initial, canManage }: { policy: RoutePolicy; canManage: boolean }) {
  const { message } = App.useApp(); const client = useQueryClient();
  const [policy, setPolicy] = useState(initial); const [conflict, setConflict] = useState(false);
  const [cursors, setCursors] = useState<(number | undefined)[]>([undefined]);
  const history = useQuery({ queryKey: ["route-history", policy.project_id, policy.model, cursors.at(-1)], queryFn: () => ops.history(policy.project_id, policy.model, cursors.at(-1)), retry: false });
  const acks = useQuery({ queryKey: ["route-acks", policy.project_id, policy.model], queryFn: () => ops.acks(policy.project_id, policy.model), refetchInterval: 5000, retry: false });
  const release = useMutation({ mutationFn: ({ action, target }: { action: "pause" | "promote" | "rollback"; target?: number }) => ops.release(policy.project_id, policy.model, policy.revision, action, target),
    onSuccess: result => { setPolicy(result); setCursors([undefined]); message.success(t("操作已提交，请等待观测更新")); },
    onError: e => { setConflict(e instanceof APIError && e.status === 409); message.error(errorMessage(e)); },
    onSettled: () => { client.invalidateQueries({ queryKey: ["route-policies"] }); client.invalidateQueries({ queryKey: ["route-history"] }); client.invalidateQueries({ queryKey: ["route-acks"] }); } });
  const disabled = !canManage || conflict || release.isPending || history.isError || acks.isError;
  return <><Alert className="page-alert" showIcon type="info" title={t("暂停将清除 Canary 权重和指向 Canary 的请求头规则。提升或回滚会创建新版本。")} />
    {conflict && <Alert className="page-alert" showIcon type="warning" title={t("策略已被其他人修改。关闭并重新打开，加载最新版本后再发布。")} />}
    {(history.isError || acks.isError) && <Alert type="error" title={errorMessage(history.error || acks.error)} />}
    <p><strong>{policy.model} · v{policy.revision}</strong></p>
    <Space className="table-toolbar">{([ ["pause", t("暂停灰度")], ["promote", t("提升 Canary")] ] as const).map(([action, label]) => <Popconfirm key={action} title={label} onConfirm={() => release.mutateAsync({ action }).catch(() => {})} disabled={disabled || !policy.spec.canary_pool}><Button disabled={disabled || !policy.spec.canary_pool} loading={release.isPending && release.variables?.action === action}>{label}</Button></Popconfirm>)}</Space>
    <Alert className="page-alert" type="info" title={t("仅统计已知 Gateway 会话，不代表全部网关已确认。")} description={<>{acks.data?.sessions.length ?? 0} · {acks.data?.all_known_acknowledged && !!acks.data.sessions.length ? t("已知会话已确认") : t("等待已知会话确认")}</>} />
    <Table size="small" rowKey="session_id" dataSource={acks.data?.sessions} columns={[
      { title: t("会话"), dataIndex: "session_id", ellipsis: true }, { title: "ACK", dataIndex: "acknowledged", render: value => <Tag color={value ? "green" : "orange"}>{String(value)}</Tag> },
    ]} />
    <Table size="small" rowKey="revision" dataSource={history.data?.data} loading={history.isPending} pagination={false} scroll={{ x: 680 }} expandable={{ expandedRowRender: row => <pre style={{ whiteSpace: "pre-wrap" }}>{JSON.stringify(row.spec, null, 2)}</pre> }} columns={[
      { title: t("已保存版本"), dataIndex: "revision", render: value => `v${value}` }, { title: t("动作"), dataIndex: "operation" },
      { title: t("执行者"), dataIndex: "actor", ellipsis: true }, { title: t("创建时间"), dataIndex: "created_at", render: formatDate },
      { title: t("操作"), render: (_, row) => <Popconfirm title={t("回滚到此版本")} onConfirm={() => release.mutateAsync({ action: "rollback", target: row.revision }).catch(() => {})} disabled={disabled || row.revision >= policy.revision}><Button disabled={disabled || row.revision >= policy.revision}>{t("回滚到此版本")}</Button></Popconfirm> },
    ]} /><Space className="table-toolbar"><Button disabled={cursors.length === 1} onClick={() => setCursors(c => c.slice(0, -1))}>{t("上一页")}</Button><Button disabled={!history.data?.next_before} onClick={() => setCursors(c => [...c, history.data!.next_before!])}>{t("下一页")}</Button></Space>
  </>;
}
