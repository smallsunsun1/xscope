import { t, useI18n, useLocalizedForm, getLocale } from "../i18n";
import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import {
  Alert,
  App,
  Button,
  Descriptions,
  Drawer,
  Form,
  Input,
  InputNumber,
  Modal,
  Popconfirm,
  Select,
  Switch,
  Table,
  Tabs,
  Tag,
} from "antd";
import { Coins, Gauge, LockKeyhole, MessageSquareText, Plus, ReceiptText, RefreshCw, Sigma, WalletCards } from "lucide-react";
import { useState } from "react";
import { api, errorMessage } from "../api";
import { MetricCard, PageHeader, ResourceEmpty } from "../components";
import { formatDate, formatMicrounits, formatMoney, formatNumber } from "../format";
import { PendingReservations } from "./PendingReservations";
import { EventWorker } from "./BillingWorkflows";
import { ops } from "../operations";
import type {
  BillingOrder,
  LedgerEntry,
  LedgerTransaction,
  Payment,
  ProjectBilling,
  ReconciliationReport,
} from "../types";

type TopUpForm = { amount_yuan: number; description: string };
type RefundForm = { amount_yuan: number; reason: string };

export function BillingPage() {
  useI18n();
  const { message } = App.useApp();
  const queryClient = useQueryClient();
  const [projectID, setProjectID] = useState("");
  const [activeTab, setActiveTab] = useState("usage");
  const [topUpOpen, setTopUpOpen] = useState(false);
  const [refundPayment, setRefundPayment] = useState<Payment>();
  const [reconciliation, setReconciliation] = useState<ReconciliationReport>();
  const [topUpForm] = Form.useForm<TopUpForm>();
  useLocalizedForm(topUpForm);
  const [refundForm] = Form.useForm<RefundForm>();
  useLocalizedForm(refundForm);
  const projects = useQuery({ queryKey: ["projects"], queryFn: api.projects });
  const project = projects.data?.find((item) => item.id === projectID);
  const session = useQuery({ queryKey: ["console-session"], queryFn: api.session, retry: false });
  const capabilities = useQuery({ queryKey: ["capabilities"], queryFn: ops.capabilities, retry: false });
  const canManage = Boolean(project && session.data?.memberships?.some(item => item.tenant_id === project.tenant_id && item.role === "owner"));
  const position = useQuery({ queryKey: ["billing-position", projectID], queryFn: () => api.billingPosition(projectID), enabled: Boolean(projectID), refetchInterval: 30_000, retry: false });
  const billing = useQuery({
    queryKey: ["billing-summary", projectID],
    queryFn: () => api.billingSummary(projectID),
    refetchInterval: 15_000,
  });
  const account = useQuery({
    queryKey: ["billing-account", projectID],
    queryFn: () => api.billingAccount(projectID),
    enabled: Boolean(projectID),
    refetchInterval: 30_000,
  });
  const orders = useQuery({ queryKey: ["billing-orders"], queryFn: api.billingOrders, enabled: ["orders", "reconciliation"].includes(activeTab), retry: false });
  const payments = useQuery({ queryKey: ["billing-payments"], queryFn: api.payments, enabled: ["orders", "reconciliation"].includes(activeTab), retry: false });
  const refunds = useQuery({ queryKey: ["billing-refunds"], queryFn: api.refunds, enabled: activeTab === "orders", retry: false });
  const ledger = useQuery({ queryKey: ["billing-ledger"], queryFn: api.ledger, enabled: activeTab === "ledger", retry: false });
  const invoices = useQuery({ queryKey: ["billing-invoices"], queryFn: api.invoices, enabled: activeTab === "invoices", retry: false });
  const visibleOrders = orders.data?.filter(row => !projectID || row.project_id === projectID);
  const orderIDs = new Set(visibleOrders?.map(row => row.id));
  const visiblePayments = payments.data?.filter(row => !projectID || orderIDs.has(row.order_id));

  const refreshFinance = async () => {
    await Promise.all([
      queryClient.invalidateQueries({ queryKey: ["billing-position"] }),
      queryClient.invalidateQueries({ queryKey: ["billing-summary"] }),
      queryClient.invalidateQueries({ queryKey: ["billing-account"] }),
      queryClient.invalidateQueries({ queryKey: ["billing-orders"] }),
      queryClient.invalidateQueries({ queryKey: ["billing-payments"] }),
      queryClient.invalidateQueries({ queryKey: ["billing-refunds"] }),
      queryClient.invalidateQueries({ queryKey: ["billing-ledger"] }),
      queryClient.invalidateQueries({ queryKey: ["billing-invoices"] }),
    ]);
  };
  const createOrder = useMutation({
    mutationFn: (values: TopUpForm) => {
      if (!project) throw new Error(t("请先选择项目"));
      return api.createOrder({
        id: `order-${crypto.randomUUID()}`,
        tenant_id: project.tenant_id,
        project_id: project.id,
        amount: { currency: "CNY", amount: Math.round(values.amount_yuan * 100) },
        description: values.description || t("控制台余额充值"),
      });
    },
    onSuccess: async () => {
      setTopUpOpen(false);
      topUpForm.resetFields();
      await refreshFinance();
      message.success(t("充值订单已创建，请确认支付入账"));
    },
    onError: (error) => message.error(errorMessage(error)),
  });
  const capture = useMutation({
    mutationFn: (order: BillingOrder) => api.capturePayment(order.id, `payment-${crypto.randomUUID()}`),
    onSuccess: async () => {
      await refreshFinance();
      message.success(t("支付已确认，余额与双分录账本已同步"));
    },
    onError: (error) => message.error(errorMessage(error)),
  });
  const createRefund = useMutation({
    mutationFn: (values: RefundForm) => {
      if (!refundPayment) throw new Error(t("请选择支付记录"));
      return api.createRefund(
        refundPayment.id,
        { currency: refundPayment.amount.currency, amount: Math.round(values.amount_yuan * 100) },
        values.reason,
      );
    },
    onSuccess: async () => {
      setRefundPayment(undefined);
      refundForm.resetFields();
      await refreshFinance();
      message.success(t("退款已完成并生成反向分录"));
    },
    onError: (error) => message.error(errorMessage(error)),
  });
  const policy = useMutation({
    mutationFn: (enabled: boolean) => api.updateBalancePolicy(projectID, enabled),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["billing-account", projectID] });
      message.success(t("余额门禁策略已更新，网关将在策略刷新后生效"));
    },
    onError: (error) => message.error(errorMessage(error)),
  });
  const issueInvoice = useMutation({
    mutationFn: () => {
      if (!project) throw new Error(t("请先选择项目"));
      const now = new Date();
      const periodStart = new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), 1));
      const periodEnd = new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth() + 1, 1));
      return api.createInvoice({
        id: `invoice-${crypto.randomUUID()}`,
        tenant_id: project.tenant_id,
        project_id: project.id,
        period_start: periodStart.toISOString(),
        period_end: periodEnd.toISOString(),
        title: t("{value0} {value1} 用量账单", { value0: project.name, value1: periodStart.toISOString().slice(0, 7) }),
      });
    },
    onSuccess: async () => {
      await refreshFinance();
      message.success(t("内部账单记录已生成（非税务发票）"));
    },
    onError: (error) => message.error(errorMessage(error)),
  });
  const runReconciliation = useMutation({
    mutationFn: () => {
      if (!project) throw new Error(t("请先选择项目"));
      return api.reconcile({
        tenant_id: project.tenant_id,
        provider: "manual",
        settlements: (payments.data ?? [])
        .filter((item) => item.provider === "manual" && item.status === "succeeded" && orders.data?.some(order => order.id === item.order_id && order.tenant_id === project.tenant_id))
        .map((item) => ({ provider_reference: item.provider_reference, amount: item.amount })),
      });
    },
    onSuccess: setReconciliation,
    onError: (error) => message.error(errorMessage(error)),
  });

  const summary = billing.data;
  const period = summary
    ? `${new Date(summary.period_start).toLocaleDateString(getLocale())} – ${new Date(summary.period_end).toLocaleDateString(getLocale())}`
    : t("本月");

  return (
    <>
      <PageHeader
        eyebrow="Finance"
        title={t("计费与财务")}
        description={t("查看真实用量、资金冻结和账务证据。人工入账用于开发验证，尚未连接正式支付渠道或税务开票。")}
        action={<Select aria-label={t("全部项目")} allowClear value={projectID || undefined} placeholder={t("全部项目")} style={{ width: 240 }} onChange={(value) => setProjectID(value ?? "")} options={projects.data?.map((item) => ({ label: item.name, value: item.id }))} />}
      />
      <div className="section-caption"><span>{t("USAGE / 本期用量")}</span><Button size="small" type="text" icon={<RefreshCw size={14} />} onClick={refreshFinance}>{t("刷新账务")}</Button></div>
      {billing.isError && <Alert className="page-alert" type="error" showIcon title={t("无法读取用量")} description={errorMessage(billing.error)} />}
      <div className="metric-grid">
        <MetricCard icon={Coins} label={t("本期费用")} value={summary ? formatMoney(summary.total.amount, summary.total.currency) : "—"} detail={period} loading={billing.isLoading} />
        <MetricCard icon={WalletCards} label={t("账面余额")} value={account.data ? formatMoney(account.data.balance.amount, account.data.balance.currency) : projectID ? "—" : t("选择项目")} detail={t("未扣除预占冻结 · 下方查看精确资金")} loading={account.isLoading} tone="blue" />
        <MetricCard icon={MessageSquareText} label={t("请求数")} value={summary ? formatNumber(summary.requests) : "—"} detail={t("已入账的推理请求")} loading={billing.isLoading} tone="amber" />
        <MetricCard icon={Sigma} label="Tokens" value={summary ? formatNumber(summary.input_tokens + summary.output_tokens) : "—"} detail={summary ? t("输入 {value0} · 输出 {value1}", { value0: formatNumber(summary.input_tokens), value1: formatNumber(summary.output_tokens) }) : t("等待用量数据")} loading={billing.isLoading} tone="violet" />
      </div>
      {projectID && <>
        <div className="section-caption"><span>FUNDS / {project?.name ?? projectID}</span><span className="muted">{t("金额保留微单位精度 · 每 30 秒刷新")}</span></div>
        {position.isError ? <Alert className="page-alert" type="error" showIcon title={t("无法读取资金明细")} description={errorMessage(position.error)} /> : <div className="funds-grid">
          <MetricCard icon={WalletCards} label={t("账面余额")} value={position.data ? formatMicrounits(position.data.balance_microunits, position.data.currency) : "—"} detail={t("已入账资金净额")} loading={position.isPending} />
          <MetricCard icon={LockKeyhole} label={t("预占冻结")} value={position.data ? formatMicrounits(position.data.held_microunits, position.data.currency) : "—"} detail={t("包含仍在执行和待核查请求，不是已扣费")} loading={position.isPending} tone="amber" />
          <MetricCard icon={Coins} label={t("可用余额")} value={position.data ? formatMicrounits(position.data.available_microunits, position.data.currency) : "—"} detail={t("账面余额 − 预占冻结；实际准入以事务为准")} loading={position.isPending} tone="blue" />
        </div>}
      </>}
      {projectID && account.data && (
        <div className="inline-notice billing-policy">
          <Gauge size={18} />
          <span><strong>{t("预付余额门禁")}</strong> {t(" 按可用资金检查新请求的最大预占额。")}</span>
          <Switch aria-label={t("预付余额门禁")} disabled={!canManage} checked={account.data.enforce_balance} loading={policy.isPending} onChange={(checked) => policy.mutate(checked)} />
        </div>
      )}
      <section className="panel table-panel billing-workbench">
        {([...(activeTab === "orders" ? [orders, payments, refunds] : []), ...(activeTab === "ledger" ? [ledger] : []), ...(activeTab === "invoices" ? [invoices] : [])].find(query => query.isError)) && <Alert className="workbench-notice" showIcon type="error" title={t("部分账务数据读取失败")} description={t("可能缺少 owner 权限或服务暂不可用；不要把空表理解为没有账务记录。")} />}
        <Tabs
          activeKey={activeTab}
          onChange={setActiveTab}
          items={[
            { key: "pending", label: t("待核查请求"), children: !projectID ? <ResourceEmpty title={t("先选择一个项目")} description={t("按项目查询冻结请求；仅租户 owner 可查看待核查记录。")} /> : !canManage && !capabilities.data?.billing_reviewer ? <Alert className="workbench-notice" type="info" showIcon title={t("需要项目所属租户的 owner 权限")} /> : <PendingReservations key={projectID} projectID={projectID} currency={position.data?.currency ?? account.data?.currency ?? "CNY"} /> },
            { key: "worker", label: t("事件消费者"), children: !projectID ? <ResourceEmpty title={t("先选择一个项目")} description={t("重试不改变 ACK，也不会自动释放未知用量冻结。")} /> : !canManage ? <Alert type="info" title={t("需要项目所属租户的 owner 权限")} /> : <EventWorker key={projectID} projectID={projectID} /> },
            {
              key: "usage",
              label: t("用量汇总"),
              children: <UsageTable loading={billing.isLoading} period={period} data={summary?.projects} />,
            },
            {
              key: "orders",
              label: t("人工入账与退款"),
              children: (
                <>
                  <div className="table-toolbar">
                    <strong>{t("充值订单")}</strong>
                    <Button type="primary" icon={<Plus size={15} />} disabled={!canManage} onClick={() => { topUpForm.setFieldsValue({ amount_yuan: 100, description: t("控制台余额充值") }); setTopUpOpen(true); }}>{t("创建充值订单")}</Button>
                  </div>
                  <Table<BillingOrder>
                    rowKey="id"
                    loading={orders.isLoading}
                    dataSource={visibleOrders}
                    scroll={{ x: 900 }}
                    pagination={{ pageSize: 5, hideOnSinglePage: true }}
                    columns={[
                      { title: t("订单"), dataIndex: "id", render: (value: string, row) => <div><code className="soft-code">{value}</code><div className="cell-subtitle">{row.description}</div></div> },
                      { title: t("项目"), dataIndex: "project_id" },
                      { title: t("金额"), dataIndex: "amount", render: (value: BillingOrder["amount"]) => formatMoney(value.amount, value.currency) },
                      { title: t("状态"), dataIndex: "status", render: (value: string) => <FinanceStatus value={value} /> },
                      { title: t("创建时间"), dataIndex: "created_at", render: formatDate },
                      { title: t("操作"), align: "right", render: (_, row) => row.status === "pending" ? <Popconfirm title={t("确认这笔支付已到账？")} description={t("初版使用 manual provider；确认后会立即写入双分录。")} onConfirm={() => capture.mutate(row)}><Button type="link" loading={capture.isPending}>{t("确认支付")}</Button></Popconfirm> : null },
                    ]}
                  />
                  <div className="table-toolbar billing-subtable"><strong>{t("支付与退款")}</strong><Tag>{refunds.data?.length ?? 0} {t(" 笔退款")}</Tag></div>
                  <Table<Payment>
                    rowKey="id"
                    loading={payments.isLoading || refunds.isLoading}
                    dataSource={visiblePayments}
                    scroll={{ x: 900 }}
                    pagination={{ pageSize: 5, hideOnSinglePage: true }}
                    columns={[
                      { title: t("支付"), dataIndex: "id", render: (value: string, row) => <div><code className="soft-code">{value}</code><div className="cell-subtitle">{row.provider} · {row.provider_reference}</div></div> },
                      { title: t("金额"), dataIndex: "amount", render: (value: Payment["amount"]) => formatMoney(value.amount, value.currency) },
                      { title: t("状态"), dataIndex: "status", render: (value: string) => <FinanceStatus value={value} /> },
                      { title: t("支付时间"), dataIndex: "paid_at", render: formatDate },
                      { title: t("操作"), align: "right", render: (_, row) => row.status === "succeeded" ? <Button type="link" onClick={() => { refundForm.setFieldsValue({ amount_yuan: row.amount.amount / 100, reason: t("客户退款") }); setRefundPayment(row); }}>{t("退款")}</Button> : null },
                    ]}
                  />
                </>
              ),
            },
            {
              key: "ledger",
              label: t("双分录账本"),
              children: <LedgerTable loading={ledger.isLoading || account.isLoading} data={ledger.data?.filter(row => !projectID || row.entries.some(entry => entry.billing_account_id === account.data?.id))} />,
            },
            {
              key: "invoices",
              label: t("内部账单"),
              children: (
                <>
                  <div className="table-toolbar"><strong>{t("内部账单 · 非税务发票")}</strong><Button icon={<ReceiptText size={15} />} disabled={!canManage} loading={issueInvoice.isPending} onClick={() => issueInvoice.mutate()}>{t("生成本月账单")}</Button></div>
                  <Table
                    rowKey="id"
                    loading={invoices.isLoading}
                    dataSource={invoices.data?.filter(row => !projectID || row.project_id === projectID)}
                    scroll={{ x: 760 }}
                    locale={{ emptyText: <ResourceEmpty title={t("暂无内部账单")} description={t("选择项目后可生成用量账单记录，不具备税务发票效力。")} /> }}
                    columns={[
                      { title: t("发票"), dataIndex: "title", render: (value: string, row) => <div><strong>{value}</strong><div className="cell-subtitle">{row.id}</div></div> },
                      { title: t("项目"), dataIndex: "project_id" },
                      { title: t("金额"), dataIndex: "amount", render: (value) => formatMoney(value.amount, value.currency) },
                      { title: t("状态"), dataIndex: "status", render: (value: string) => <FinanceStatus value={value} /> },
                      { title: t("开具时间"), dataIndex: "issued_at", render: formatDate },
                    ]}
                  />
                </>
              ),
            },
            {
              key: "reconciliation",
              label: t("对账演示"),
              children: (
                <div className="reconciliation-panel">
                  <Alert showIcon type="info" message={t("对账适配器")} description={t("当前 manual provider 使用平台支付记录模拟渠道结算单；正式支付渠道只需提交同一 provider_reference + amount 契约。")} />
                  <Button icon={<RefreshCw size={15} />} disabled={!project} loading={runReconciliation.isPending} onClick={() => runReconciliation.mutate()}>{t("执行 manual 渠道对账")}</Button>
                  {reconciliation && <Descriptions bordered size="small" column={{ xs: 1, sm: 2 }} items={[
                    { key: "provider", label: t("渠道"), children: reconciliation.provider },
                    { key: "matched", label: t("匹配"), children: t("{value0} 笔", { value0: reconciliation.matched }) },
                    { key: "platform", label: t("仅平台存在"), children: reconciliation.platform_only.length || "0" },
                    { key: "provider-only", label: t("仅渠道存在"), children: reconciliation.provider_only.length || "0" },
                    { key: "mismatch", label: t("金额不一致"), children: Object.keys(reconciliation.amount_mismatches).length || "0" },
                    { key: "at", label: t("执行时间"), children: formatDate(reconciliation.generated_at) },
                  ]} />}
                </div>
              ),
            },
          ]}
        />
      </section>
      <Drawer title={t("创建充值订单")} size={480} open={topUpOpen} onClose={() => setTopUpOpen(false)} extra={<Button type="primary" loading={createOrder.isPending} onClick={() => topUpForm.submit()}>{t("创建订单")}</Button>}>
        <Form form={topUpForm} layout="vertical" onFinish={(values) => createOrder.mutate(values)}>
          <Form.Item label={t("项目")}><Input value={project ? `${project.name} · ${project.id}` : t("未选择")} disabled /></Form.Item>
          <Form.Item name="amount_yuan" label={t("充值金额")} rules={[{ required: true, message: t("请输入金额") }]}><InputNumber min={0.01} precision={2} prefix="¥" style={{ width: "100%" }} /></Form.Item>
          <Form.Item name="description" label={t("订单备注")}><Input.TextArea rows={3} maxLength={200} /></Form.Item>
        </Form>
      </Drawer>
      <Modal title={t("发起退款")} open={Boolean(refundPayment)} onCancel={() => setRefundPayment(undefined)} onOk={() => refundForm.submit()} confirmLoading={createRefund.isPending} okText={t("确认退款")}>
        <Form form={refundForm} layout="vertical" onFinish={(values) => createRefund.mutate(values)}>
          <Form.Item label={t("原支付")}><Input value={refundPayment?.id} disabled /></Form.Item>
          <Form.Item name="amount_yuan" label={t("退款金额")} rules={[{ required: true, message: t("请输入退款金额") }]}><InputNumber min={0.01} max={(refundPayment?.amount.amount ?? 0) / 100} precision={2} prefix="¥" style={{ width: "100%" }} /></Form.Item>
          <Form.Item name="reason" label={t("退款原因")} rules={[{ required: true, message: t("请输入退款原因") }]}><Input.TextArea rows={3} /></Form.Item>
        </Form>
      </Modal>
    </>
  );
}

function UsageTable({ loading, period, data }: { loading: boolean; period: string; data?: ProjectBilling[] }) {
  useI18n();
  return (
    <>
      <div className="table-toolbar"><strong>{t("项目费用明细")}</strong><Tag>{period}</Tag></div>
      <Table<ProjectBilling>
        scroll={{ x: 760 }}
        rowKey="project_id"
        loading={loading}
        dataSource={data}
        pagination={false}
        locale={{ emptyText: <ResourceEmpty title={t("本期暂无用量")} description={t("通过网关完成推理请求后，用量会自动进入这里。")} /> }}
        columns={[
          { title: t("项目"), dataIndex: "project_id", render: (value: string) => <code className="soft-code">{value}</code> },
          { title: t("请求数"), dataIndex: "requests", render: formatNumber },
          { title: t("输入 Tokens"), dataIndex: "input_tokens", render: formatNumber },
          { title: t("输出 Tokens"), dataIndex: "output_tokens", render: formatNumber },
          { title: t("费用"), dataIndex: "cost", align: "right", render: (money: ProjectBilling["cost"]) => <strong>{formatMoney(money.amount, money.currency)}</strong> },
        ]}
      />
    </>
  );
}

function LedgerTable({ loading, data }: { loading: boolean; data?: LedgerTransaction[] }) {
  useI18n();
  return (
    <Table<LedgerTransaction>
      scroll={{ x: 850 }}
      rowKey="id"
      loading={loading}
      dataSource={data}
      expandable={{
        expandedRowRender: (transaction) => <Table<LedgerEntry>
          rowKey="id"
          size="small"
          pagination={false}
          dataSource={transaction.entries}
          columns={[
            { title: t("账本科目"), dataIndex: "ledger_account" },
            { title: t("账户"), dataIndex: "billing_account_id" },
            { title: t("精确金额"), dataIndex: "amount_microunits", align: "right", render: (value: number, row) => <span className={value < 0 ? "amount-negative" : "amount-positive"}>{value > 0 ? "+" : ""}{formatMoney(value / 1_000_000, row.currency)}</span> },
          ]}
        />,
      }}
      locale={{ emptyText: <ResourceEmpty title={t("账本暂无分录")} description={t("用量入账、支付与退款都会生成总和为零的双分录。")} /> }}
      columns={[
        { title: t("交易"), dataIndex: "id", render: (value: string, row) => <div><code className="soft-code">{value}</code><div className="cell-subtitle">{row.description}</div></div> },
        { title: t("类型"), dataIndex: "kind", render: (value: string) => <FinanceStatus value={value} /> },
        { title: t("业务引用"), render: (_, row) => `${row.reference_type} · ${row.reference_id}` },
        { title: t("分录数"), dataIndex: "entries", render: (entries: LedgerEntry[]) => entries.length },
        { title: t("时间"), dataIndex: "created_at", render: formatDate },
      ]}
    />
  );
}

function FinanceStatus({ value }: { value: string }) {
  useI18n();
  const colors: Record<string, string> = {
    succeeded: "success",
    paid: "success",
    issued: "blue",
    pending: "warning",
    refund: "purple",
    top_up: "cyan",
    usage: "gold",
  };
  const labels: Record<string, string> = {
    succeeded: t("已成功"), paid: t("已支付"), issued: t("已开具"),
    pending: t("待处理"), refund: t("退款"), top_up: t("充值"), usage: t("用量"),
  };
  return <Tag color={colors[value]}>{labels[value] ?? value}</Tag>;
}
