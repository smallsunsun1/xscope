import { useState } from "react";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { Alert, App, Button, Checkbox, Descriptions, Drawer, Form, Input, InputNumber, Popconfirm, Select, Space, Table, Tabs, Tag } from "antd";
import { APIError, api, errorMessage } from "../api";
import { t, useLocalizedForm } from "../i18n";
import { formatDate } from "../format";
import { ops, type Evidence, type Review } from "../operations";

export function EventWorker({ projectID }: { projectID: string }) {
  const { message } = App.useApp(); const [reason, setReason] = useState("");
  const worker = useQuery({ queryKey: ["event-worker", projectID], queryFn: () => ops.worker(projectID), refetchInterval: 15000, retry: false });
  const retry = useMutation({ mutationFn: () => ops.retryWorker(projectID, reason), onSuccess: () => { setReason(""); worker.refetch(); message.success(t("操作已提交，请等待观测更新")); }, onError: e => message.error(errorMessage(e)) });
  if (worker.isError) return <Alert type="warning" title={worker.error instanceof APIError && worker.error.status === 404 ? t("暂无数据") : errorMessage(worker.error)} />;
  return <><Alert className="page-alert" type="info" showIcon title={t("重试不改变 ACK，也不会自动释放未知用量冻结。")} />
    <Descriptions bordered column={2} items={[
      { key: "consumer", label: t("事件消费者"), children: worker.data?.consumer }, { key: "state", label: t("状态"), children: worker.data?.state },
      { key: "backlog", label: t("积压事件"), children: worker.data?.backlog }, { key: "ack", label: "ACK / Target", children: `${worker.data?.acknowledged ?? "—"} / ${worker.data?.target_sequence ?? "—"}` },
      { key: "attempts", label: t("重试次数"), children: worker.data?.attempts }, { key: "available", label: t("下次可领取"), children: formatDate(worker.data?.available_at) },
      { key: "error", label: t("错误码"), children: worker.data?.last_error || "—" }, { key: "updated", label: t("最后更新"), children: formatDate(worker.data?.updated_at) },
    ]} /><Space className="table-toolbar" wrap><Input aria-label={t("处理说明")} placeholder={t("处理说明")} value={reason} maxLength={2048} onChange={e => setReason(e.target.value)} disabled={worker.data?.state !== "dead" || retry.isPending} />
      <Popconfirm title={t("重试死信任务")} onConfirm={() => retry.mutateAsync().catch(() => {})} disabled={worker.data?.state !== "dead" || !reason.trim() || retry.isPending}><Button disabled={worker.data?.state !== "dead" || !reason.trim()} loading={retry.isPending}>{t("重试死信任务")}</Button></Popconfirm><Button onClick={() => worker.refetch()}>{t("刷新列表")}</Button></Space></>;
}

export function BillingReviews({ projectID, reservationID }: { projectID: string; reservationID: string }) {
  const { message } = App.useApp(); const client = useQueryClient();
  const capabilities = useQuery({ queryKey: ["capabilities"], queryFn: ops.capabilities, retry: false });
  const session = useQuery({ queryKey: ["console-session"], queryFn: api.session, retry: false });
  const projects = useQuery({ queryKey: ["projects"], queryFn: api.projects });
  const tenant = projects.data?.find(p => p.id === projectID)?.tenant_id;
  const owner = !!tenant && !!session.data?.memberships?.some(m => m.tenant_id === tenant && m.role === "owner");
  const [cursors, setCursors] = useState<(string | undefined)[]>([undefined]); const [selected, setSelected] = useState<Review>();
  const [mode, setMode] = useState<"evidence" | "waiver">(); const [reason, setReason] = useState("");
  const reviews = useQuery({ queryKey: ["billing-reviews", projectID, reservationID, cursors.at(-1)], queryFn: () => ops.reviews(projectID, reservationID, cursors.at(-1)), retry: false });
  const detail = useQuery({ queryKey: ["review-detail", projectID, selected?.id], queryFn: () => ops.review(projectID, selected!.id), enabled: !!selected, retry: false });
  const refresh = () => { client.invalidateQueries({ queryKey: ["billing-reviews"] }); client.invalidateQueries({ queryKey: ["review-detail"] }); client.invalidateQueries({ queryKey: ["billing-pending"] }); client.invalidateQueries({ queryKey: ["billing-position"] }); };
  const decide = useMutation({ mutationFn: (action: string) => ops.decide(projectID, detail.data!, action, reason), onSuccess: () => { setSelected(undefined); refresh(); message.success(t("核查决定已保存")); }, onError: e => { message.error(errorMessage(e)); refresh(); } });
  const canDecide = capabilities.data?.billing_reviewer && !!session.data?.id && detail.data?.submitted_by !== session.data.id && detail.data?.state === "submitted" && !detail.isError && !!reason.trim() && !decide.isPending;
  return <><Alert className="page-alert" type="warning" showIcon title={t("无证据时继续冻结；损失豁免必须由两位独立财务审核员确认。")} description={t("仅租户 owner 可提交用量证据；仅授权财务审核员可审批。")} />
    <Space className="table-toolbar" wrap><Button type="primary" disabled={!owner || !capabilities.data?.evidence_submission || capabilities.isError} onClick={() => setMode("evidence")}>{t("提交用量证据")}</Button><Button disabled={!capabilities.data?.billing_reviewer || capabilities.isError} onClick={() => setMode("waiver")}>{t("申请损失豁免")}</Button><Button onClick={refresh}>{t("刷新列表")}</Button></Space>
    {reviews.isError && <Alert type="error" title={errorMessage(reviews.error)} />}
    <Table rowKey="id" dataSource={reviews.data?.data} loading={reviews.isPending} size="small" pagination={false} scroll={{ x: 600 }} columns={[
      { title: t("请求"), dataIndex: "id", ellipsis: true }, { title: t("状态"), dataIndex: "state", render: value => <Tag>{value}</Tag> }, { title: t("创建时间"), dataIndex: "created_at", render: formatDate },
      { title: t("操作"), render: (_, row) => <Button onClick={() => { setReason(""); setSelected(row); }}>{t("查看证据")}</Button> },
    ]} /><Space className="table-toolbar"><Button disabled={cursors.length === 1} onClick={() => setCursors(c => c.slice(0, -1))}>{t("上一页")}</Button><Button disabled={!reviews.data?.next} onClick={() => setCursors(c => [...c, reviews.data!.next!])}>{t("下一页")}</Button></Space>
    <Drawer title={t("查看证据")} open={!!selected} size={660} destroyOnHidden onClose={() => !decide.isPending && setSelected(undefined)}>
      {detail.isError && <Alert type="error" title={errorMessage(detail.error)} />}
      {detail.data && !detail.isError && <><Alert showIcon type="warning" title={t("审批绑定当前证据摘要，批准后执行幂等补结算或已声明的损失豁免。")} />
        <p style={{ overflowWrap: "anywhere" }}>SHA-256: {detail.data.evidence_sha256}</p>
        <Tabs items={[{ key: "evidence", label: t("查看证据"), children: <pre className="evidence-document">{JSON.stringify(detail.data.evidence, null, 2)}</pre> }, { key: "audit", label: t("核查与审计"), children: <pre className="evidence-document">{JSON.stringify(detail.data.audit, null, 2)}</pre> }]} />
        {detail.data.submitted_by === session.data?.id && <Alert type="info" title={t("不能审批自己提交的证据")} />}
        <Input.TextArea aria-label={t("处理说明")} rows={3} maxLength={2048} value={reason} onChange={e => setReason(e.target.value)} disabled={!capabilities.data?.billing_reviewer || decide.isPending} />
        <Space className="table-toolbar">{([ ["approve", t("批准证据")], ["reject", t("驳回证据")] ] as const).map(([action, label]) => <Popconfirm key={action} title={label} onConfirm={() => decide.mutateAsync(action).catch(() => {})} disabled={!canDecide}><Button danger={action === "approve"} disabled={!canDecide} loading={decide.isPending}>{label}</Button></Popconfirm>)}</Space>
      </>}
    </Drawer>
    <Drawer title={mode === "waiver" ? t("申请损失豁免") : t("提交用量证据")} open={!!mode} size={640} destroyOnHidden onClose={() => setMode(undefined)}>{mode && <Submission key={mode} mode={mode} project={projectID} reservation={reservationID} done={() => { setMode(undefined); refresh(); }} />}</Drawer>
  </>;
}
function Submission({ mode, project, reservation, done }: { mode: "evidence" | "waiver"; project: string; reservation: string; done: () => void }) {
  const { message } = App.useApp(); const [id] = useState(() => `review-${crypto.randomUUID()}`);
  const [form] = Form.useForm(); useLocalizedForm(form);
  const save = useMutation({ mutationFn: (values: Record<string, unknown>) => mode === "evidence" ? ops.evidence(project, reservation, { ...values, id } as Evidence) : ops.waiver(project, reservation, { ...values, id } as Parameters<typeof ops.waiver>[2]),
    onSuccess: () => { message.success(t("提交成功，等待独立核查")); done(); }, onError: e => message.error(errorMessage(e)) });
  const required = [{ required: true, whitespace: true }];
  return <><Alert className="page-alert" type="info" showIcon title={t("提交后仍需独立审批，不会立即扣款或解冻。")} description={t("请勿上传密钥、Cookie 或含凭据的 URL；仅提交必要的用量原始记录。")} />
    <Form form={form} layout="vertical" disabled={save.isPending} onFinish={values => save.mutate(values)}>
      {mode === "evidence" ? <>
        <Form.Item name="source" label={t("证据来源")} rules={required}><Input maxLength={128} /></Form.Item>
        <Form.Item name="source_request_id" label={t("供应商请求 ID")} rules={required}><Input maxLength={128} /></Form.Item>
        <Form.Item name="document" label={t("原始用量凭证")} rules={required}><Input.TextArea rows={5} maxLength={65536} /></Form.Item>
        <Form.Item name="explanation" label={t("证据说明")} rules={required}><Input.TextArea rows={2} maxLength={2048} /></Form.Item>
        {([ ["input_tokens", t("输入 token")], ["output_tokens", t("输出 token")], ["latency_ms", t("延迟（毫秒）")] ] as const).map(([name, label]) => <Form.Item key={name} name={["usage", name]} label={label} rules={[{ required: true, type: "integer", min: 0, max: Number.MAX_SAFE_INTEGER }]}><InputNumber min={0} max={Number.MAX_SAFE_INTEGER} precision={0} /></Form.Item>)}
        <Form.Item name={["usage", "endpoint_id"]} label={t("Runtime 端点 ID")} rules={required}><Input maxLength={128} /></Form.Item>
        <Form.Item name={["usage", "region"]} label={t("区域")} rules={required}><Input maxLength={128} /></Form.Item>
        <Form.Item name={["usage", "status"]} label={t("最终状态")} rules={[{ required: true }]}><Select options={["succeeded", "cancelled", "provider_error"].map(value => ({ value, label: value }))} /></Form.Item>
      </> : <>
        <Form.Item name="incident_reference" label={t("事故编号")} rules={required}><Input maxLength={128} /></Form.Item>
        <Form.Item name="reason" label={t("处理说明")} rules={required}><Input.TextArea rows={3} maxLength={2048} /></Form.Item>
        {([ ["request_terminated", t("已确认请求终止")], ["platform_absorbs_loss", t("平台明确承担未计量损失")] ] as const).map(([name, label]) => <Form.Item key={name} name={name} valuePropName="checked" rules={[{ validator: (_, value) => value === true ? Promise.resolve() : Promise.reject(new Error(label)) }]}><Checkbox>{label}</Checkbox></Form.Item>)}
      </>}
      <Button type="primary" htmlType="submit" loading={save.isPending}>{mode === "evidence" ? t("提交用量证据") : t("申请损失豁免")}</Button>
    </Form></>;
}
