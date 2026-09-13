import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Alert, App, Button, Descriptions, Drawer, Input, Popconfirm, Select, Space, Table, Tabs, Tag } from "antd";
import { PageHeader } from "../components";
import { errorMessage } from "../api";
import { formatDate } from "../format";
import { t, useI18n } from "../i18n";
import { ops, type ManagedPool, type OpsAlert, type Audit } from "../operations";

export function OperationsPage() {
  useI18n();
  const capabilities = useQuery({ queryKey: ["capabilities"], queryFn: ops.capabilities, retry: false });
  return <><PageHeader eyebrow="OPERATIONS" title={t("运行与恢复")} description={t("受管池、集群同步、告警通知与审计。状态未知时保留安全门禁。")} />
    {capabilities.isError ? <Alert type="error" title={errorMessage(capabilities.error)} /> : capabilities.isPending ? <div aria-busy="true">{t("正在连接")}</div> : !capabilities.data.platform_admin ?
      <Alert showIcon type="warning" title={t("需要平台管理员权限")} description={t("租户 owner 不自动拥有跨集群管理权限。请联系平台管理员配置授权。")} /> :
      <Tabs destroyOnHidden items={[
        { key: "pools", label: t("受管模型池"), children: <Pools /> },
        { key: "clusters", label: t("成员集群"), children: <Clusters /> },
        { key: "alerts", label: t("告警中心"), children: <Alerts configured={capabilities.data.alert_receiver_configured} /> },
        { key: "backup", label: t("备份与恢复"), children: <Backup /> },
        { key: "audit", label: t("操作审计"), children: <Audits /> },
      ]} />}</>;
}
function Pools() {
  const pools = useQuery({ queryKey: ["managed-pools"], queryFn: ops.pools, refetchInterval: 10000, retry: false });
  const [selected, setSelected] = useState<string>();
  return <><Alert className="page-alert" showIcon type="info" title={t("整池停止新请求，已有请求继续执行。请先确认其他 Pool 可以承接流量。")} />
    {pools.isError && <Alert type="error" title={errorMessage(pools.error)} action={<Button onClick={() => pools.refetch()}>{t("重试")}</Button>} />}
    <section className="table-card"><Table<ManagedPool> rowKey="id" dataSource={pools.data?.data} loading={pools.isPending} scroll={{ x: 760 }} locale={{ emptyText: t("没有受管池；旧入口不自动获得安全缩容保证。") }} columns={[
      { title: "Pool", dataIndex: "id", render: id => <Button type="link" onClick={() => setSelected(id)}>{id}</Button> },
      { title: t("成员集群"), dataIndex: "cluster_id" }, { title: t("模型部署"), dataIndex: "deployment" },
      { title: t("代次"), dataIndex: "generation" }, { title: t("状态"), dataIndex: "state", render: state => <Tag color={state === "active" ? "green" : "orange"}>{state}</Tag> },
      { title: t("操作"), render: (_, row) => <Button onClick={() => setSelected(row.id)}>{t("查看排空状态")}</Button> },
    ]} /></section>
    <Drawer title={t("查看排空状态")} size={780} open={!!selected} destroyOnHidden onClose={() => setSelected(undefined)}>{selected && <PoolDetail key={selected} id={selected} />}</Drawer></>;
}
function PoolDetail({ id }: { id: string }) {
  const { message } = App.useApp(); const client = useQueryClient();
  const detail = useQuery({ queryKey: ["pool-status", id], queryFn: () => ops.pool(id), refetchInterval: 5000, retry: false });
  const change = useMutation({ mutationFn: ({ action, generation }: { action: "drain" | "finish-drain" | "activate"; generation: number }) => ops.transition(id, generation, action),
    onSuccess: () => message.success(t("操作已提交，请等待观测更新")), onError: e => message.error(errorMessage(e)),
    onSettled: () => { client.invalidateQueries({ queryKey: ["pool-status", id] }); client.invalidateQueries({ queryKey: ["managed-pools"] }); } });
  const row = detail.data;
  if (!row || detail.isError) return <Alert type={detail.isError ? "error" : "info"} title={detail.isError ? errorMessage(detail.error) : t("正在连接")} action={<Button onClick={() => detail.refetch()}>{t("刷新列表")}</Button>} />;
  const fresh = !!row.ready_until && Date.parse(row.ready_until) > Date.now();
  const busy = !!row.scale_operation || change.isPending;
  return <><Descriptions column={2} bordered items={[
    { key: "pool", label: "Pool", children: row.id, span: 2 },
    { key: "state", label: t("状态"), children: `${row.state} · g${row.generation}` },
    { key: "version", label: t("配置版本"), children: `v${row.desired_version}` },
    { key: "ready", label: t("实际观测"), children: <Tag color={fresh ? "green" : "orange"}>{fresh ? t("就绪证据有效") : t("就绪证据已过期或缺失")}</Tag>, span: 2 },
    { key: "observed", label: t("最后更新"), children: formatDate(row.observed_at ?? undefined) },
    { key: "code", label: t("错误码"), children: row.observation_code },
    { key: "replicas", label: t("副本数"), children: row.desired_replicas },
    { key: "blocked", label: t("阻塞会话"), children: row.blocked_participants },
    { key: "auto", label: t("自动扩缩容"), children: row.scale_operation ? `${row.scale_operation.phase} · ${row.scale_operation.from} → ${row.scale_operation.target}` : "—", span: 2 },
  ]} />
    {row.scale_operation && <Alert className="page-alert" showIcon type="info" title={t("自动操作进行中，手动操作已禁用")} />}
    <Space wrap className="table-toolbar">{([
      ["drain", t("开始排空"), !["active", "pending"].includes(row.state)],
      ["finish-drain", t("完成排空"), !row.can_finish_drain],
      ["activate", t("恢复入口"), row.state !== "drained" || !fresh],
    ] as const).map(([action, label, disabled]) => <Popconfirm key={action} title={label} description={t("服务端将再次核验代次、会话和就绪证据；确认不代表强制执行。")} onConfirm={() => change.mutateAsync({ action, generation: row.generation }).catch(() => {})} disabled={disabled || busy}><Button disabled={disabled || busy} loading={change.isPending}>{label}</Button></Popconfirm>)}</Space>
    <Table rowKey="session_id" size="small" dataSource={row.participants} scroll={{ x: 650 }} columns={[
      { title: t("会话"), dataIndex: "session_id", ellipsis: true }, { title: "ACK", dataIndex: "acknowledged_generation" },
      { title: t("在途请求"), dataIndex: "active_requests", render: count => count < 0 ? <Tag color="red">{t("未知，继续阻止缩容")}</Tag> : count },
      { title: t("状态"), dataIndex: "retired", render: retired => retired ? t("已退休") : "—" },
      { title: t("最后更新"), dataIndex: "reported_at", render: value => formatDate(value ?? undefined) },
    ]} />
  </>;
}
function Clusters() {
  const clusters = useQuery({ queryKey: ["member-clusters"], queryFn: ops.clusters, refetchInterval: 15000, retry: false });
  return <><Alert className="page-alert" showIcon type="info" title={t("ACK 只表示配置应用，不表示模型就绪。实际就绪请查看受管池观测。")} />
    {clusters.isError && <Alert type="error" title={errorMessage(clusters.error)} />}
    <section className="table-card"><Table rowKey="id" dataSource={clusters.data?.data} loading={clusters.isPending} scroll={{ x: 900 }} columns={[
      { title: t("成员集群"), dataIndex: "id" }, { title: "Namespace", dataIndex: "namespace" },
      { title: t("状态"), render: (_, row) => <Tag color={row.online && !row.revoked_at ? "green" : "orange"}>{row.online && !row.revoked_at ? t("心跳在线") : t("离线或已撤销")}</Tag> },
      { title: t("期望 / ACK"), render: (_, row) => `${row.desired_version} / ${row.acknowledged_version}` },
      { title: t("最近心跳"), dataIndex: "heartbeat_at", render: value => formatDate(value ?? undefined) },
      { title: t("凭据到期"), dataIndex: "expires_at", render: formatDate }, { title: t("错误码"), dataIndex: "last_error", render: value => value || "—" },
    ]} /></section></>;
}
function Alerts({ configured }: { configured: boolean }) {
  const { message } = App.useApp();
  const [state, setState] = useState("firing"); const [cursors, setCursors] = useState<(string | undefined)[]>([undefined]);
  const [selected, setSelected] = useState<OpsAlert>(); const [reason, setReason] = useState("");
  const alerts = useQuery({ queryKey: ["ops-alerts", state, cursors.at(-1)], queryFn: () => ops.alerts(state, cursors.at(-1)), refetchInterval: 15000, retry: false });
  const ack = useMutation({ mutationFn: () => ops.acknowledge(selected!.id, reason), onSuccess: () => { setSelected(undefined); alerts.refetch(); }, onError: e => message.error(errorMessage(e)) });
  return <><Alert className="page-alert" type={configured ? "info" : "warning"} showIcon title={configured ? t("站内通知由 Alertmanager 投递；短信、邮件和飞书未配置。") : t("告警接收器未配置")} />
    <Space className="table-toolbar"><Select aria-label={t("告警中心")} value={state} onChange={value => { setState(value); setCursors([undefined]); }} options={[{ value: "firing", label: t("待处理告警") }, { value: "resolved", label: t("已恢复告警") }]} /><Button onClick={() => alerts.refetch()}>{t("刷新列表")}</Button></Space>
    {alerts.isError && <Alert type="error" title={errorMessage(alerts.error)} />}
    <section className="table-card"><Table rowKey="id" dataSource={alerts.data?.data} loading={alerts.isPending} pagination={false} scroll={{ x: 720 }} columns={[
      { title: t("告警中心"), dataIndex: "name" }, { title: t("状态"), dataIndex: "severity", render: value => <Tag color={value === "critical" ? "red" : "orange"}>{value}</Tag> },
      { title: t("说明"), dataIndex: "summary", ellipsis: true }, { title: t("通知时间"), dataIndex: "received_at", render: formatDate },
      { title: t("操作"), render: (_, row) => <Button onClick={() => { setReason(""); setSelected(row); }}>{row.acknowledged_at ? t("已确认") : t("查看通知")}</Button> },
    ]} /></section><Space className="table-toolbar"><Button disabled={cursors.length === 1} onClick={() => setCursors(c => c.slice(0, -1))}>{t("上一页")}</Button><Button disabled={!alerts.data?.next} onClick={() => setCursors(c => [...c, alerts.data!.next!])}>{t("下一页")}</Button></Space>
    <Drawer title={t("查看通知")} open={!!selected} onClose={() => !ack.isPending && setSelected(undefined)} destroyOnHidden>{selected && <>
      <h3>{selected.name}</h3><p>{selected.summary}</p><p>{formatDate(selected.starts_at)} · {selected.state}</p>
      <Alert showIcon type="info" title={t("确认只记录接手处理，不会消除告警或释放资金。")} />
      <Input.TextArea aria-label={t("处理说明")} value={reason} onChange={e => setReason(e.target.value)} maxLength={1024} rows={4} style={{ margin: "20px 0" }} disabled={!!selected.acknowledged_at} />
      <Button type="primary" disabled={!reason.trim() || !!selected.acknowledged_at || alerts.isError} loading={ack.isPending} onClick={() => ack.mutate()}>{selected.acknowledged_at ? t("已确认") : t("确认收到")}</Button>
    </>}</Drawer></>;
}
function Audits() {
  const [cursors, setCursors] = useState<(string | undefined)[]>([undefined]); const [selected, setSelected] = useState<Audit>();
  const audit = useQuery({ queryKey: ["ops-audits", cursors.at(-1)], queryFn: () => ops.audits(cursors.at(-1)), retry: false });
  return <><Alert className="page-alert" showIcon type="info" title={t("审计记录包含意图和结果；缺少结果表示未知，不表示操作未发生。")} />
    {audit.isError && <Alert type="error" title={errorMessage(audit.error)} />}
    <section className="table-card"><Table rowKey="id" pagination={false} dataSource={audit.data?.data} loading={audit.isPending} scroll={{ x: 800 }} columns={[
      { title: t("动作"), dataIndex: "action" }, { title: t("执行者"), dataIndex: "actor_id", ellipsis: true }, { title: t("资源"), dataIndex: "resource_id", ellipsis: true },
      { title: t("创建时间"), dataIndex: "created_at", render: formatDate }, { title: t("操作"), render: (_, row) => <Button onClick={() => setSelected(row)}>{t("查看详情")}</Button> },
    ]} /></section><Space className="table-toolbar"><Button disabled={cursors.length === 1} onClick={() => setCursors(c => c.slice(0, -1))}>{t("上一页")}</Button><Button disabled={!audit.data?.next_after} onClick={() => setCursors(c => [...c, audit.data!.next_after!])}>{t("下一页")}</Button><Button onClick={() => audit.refetch()}>{t("刷新列表")}</Button></Space>
    <Drawer title={t("操作审计")} open={!!selected} onClose={() => setSelected(undefined)}><pre style={{ whiteSpace: "pre-wrap", overflowWrap: "anywhere" }}>{JSON.stringify(selected, null, 2)}</pre></Drawer></>;
}
function Backup() {
  const backup = useQuery({ queryKey: ["business-backup"], queryFn: ops.backup, refetchInterval: 30000, retry: false });
  const date = (value: number | null | undefined) => value ? formatDate(new Date(value * 1000).toISOString()) : "—";
  return <><Alert className="page-alert" type="info" showIcon title={t("每日业务快照保存在独立 PVC；不是异地备份或时间点恢复。定时任务成功不代表已完成恢复演练。")} />
    {backup.isError && <Alert className="page-alert" type="error" title={errorMessage(backup.error)} />}
    {backup.data && !backup.data.telemetry_present && <Alert className="page-alert" type="warning" showIcon title={t("备份监控暂无样本，不能判定备份正常")} />}
    <Descriptions bordered column={2} items={[
      { key: "success", label: t("最近定时成功"), children: date(backup.data?.last_successful_at) },
      { key: "scheduled", label: t("最近调度"), children: date(backup.data?.last_scheduled_at) },
      { key: "active", label: t("运行中任务"), children: backup.data?.active_jobs ?? "—" },
      { key: "suspend", label: t("调度已暂停"), children: backup.data?.suspended == null ? "—" : String(backup.data.suspended === 1) },
    ]} /><Space className="table-toolbar"><Button onClick={() => backup.refetch()}>{t("刷新列表")}</Button></Space>
    <p>{t("恢复演练只写入隔离实例，不覆盖在线数据库。请通过 Bazel 运维入口执行。")}</p><pre className="evidence-document">bazel run //tools:reliability_local -- restore-drill</pre></>;
}
