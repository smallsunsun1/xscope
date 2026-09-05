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
import { Coins, Gauge, MessageSquareText, Plus, ReceiptText, RefreshCw, Sigma, WalletCards } from "lucide-react";
import { useState } from "react";
import { api, errorMessage } from "../api";
import { MetricCard, PageHeader, ResourceEmpty } from "../components";
import { formatDate, formatMoney, formatNumber } from "../format";
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
  const { message } = App.useApp();
  const queryClient = useQueryClient();
  const [projectID, setProjectID] = useState("");
  const [topUpOpen, setTopUpOpen] = useState(false);
  const [refundPayment, setRefundPayment] = useState<Payment>();
  const [reconciliation, setReconciliation] = useState<ReconciliationReport>();
  const [topUpForm] = Form.useForm<TopUpForm>();
  const [refundForm] = Form.useForm<RefundForm>();
  const projects = useQuery({ queryKey: ["projects"], queryFn: api.projects });
  const project = projects.data?.find((item) => item.id === projectID);
  const billing = useQuery({
    queryKey: ["billing-summary", projectID],
    queryFn: () => api.billingSummary(projectID),
    refetchInterval: 15_000,
  });
  const account = useQuery({
    queryKey: ["billing-account", projectID],
    queryFn: () => api.billingAccount(projectID),
    enabled: Boolean(projectID),
  });
  const orders = useQuery({ queryKey: ["billing-orders"], queryFn: api.billingOrders });
  const payments = useQuery({ queryKey: ["billing-payments"], queryFn: api.payments });
  const refunds = useQuery({ queryKey: ["billing-refunds"], queryFn: api.refunds });
  const ledger = useQuery({ queryKey: ["billing-ledger"], queryFn: api.ledger });
  const invoices = useQuery({ queryKey: ["billing-invoices"], queryFn: api.invoices });

  const refreshFinance = async () => {
    await Promise.all([
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
      if (!project) throw new Error("请先选择项目");
      return api.createOrder({
        id: `order-${crypto.randomUUID()}`,
        tenant_id: project.tenant_id,
        project_id: project.id,
        amount: { currency: "CNY", amount: Math.round(values.amount_yuan * 100) },
        description: values.description || "控制台余额充值",
      });
    },
    onSuccess: async () => {
      setTopUpOpen(false);
      topUpForm.resetFields();
      await refreshFinance();
      message.success("充值订单已创建，请确认支付入账");
    },
    onError: (error) => message.error(errorMessage(error)),
  });
  const capture = useMutation({
    mutationFn: (order: BillingOrder) => api.capturePayment(order.id, `payment-${crypto.randomUUID()}`),
    onSuccess: async () => {
      await refreshFinance();
      message.success("支付已确认，余额与双分录账本已同步");
    },
    onError: (error) => message.error(errorMessage(error)),
  });
  const createRefund = useMutation({
    mutationFn: (values: RefundForm) => {
      if (!refundPayment) throw new Error("请选择支付记录");
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
      message.success("退款已完成并生成反向分录");
    },
    onError: (error) => message.error(errorMessage(error)),
  });
  const policy = useMutation({
    mutationFn: (enabled: boolean) => api.updateBalancePolicy(projectID, enabled),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["billing-account", projectID] });
      message.success("余额门禁策略已更新，网关将在策略刷新后生效");
    },
    onError: (error) => message.error(errorMessage(error)),
  });
  const issueInvoice = useMutation({
    mutationFn: () => {
      if (!project) throw new Error("请先选择项目");
      const now = new Date();
      const periodStart = new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), 1));
      const periodEnd = new Date(Date.UTC(now.getUTCFullYear(), now.getUTCMonth() + 1, 1));
      return api.createInvoice({
        id: `invoice-${crypto.randomUUID()}`,
        tenant_id: project.tenant_id,
        project_id: project.id,
        period_start: periodStart.toISOString(),
        period_end: periodEnd.toISOString(),
        title: `${project.name} ${periodStart.toISOString().slice(0, 7)} 用量账单`,
      });
    },
    onSuccess: async () => {
      await refreshFinance();
      message.success("发票记录已开具");
    },
    onError: (error) => message.error(errorMessage(error)),
  });
  const runReconciliation = useMutation({
    mutationFn: () => {
      if (!project) throw new Error("请先选择项目");
      return api.reconcile({
        tenant_id: project.tenant_id,
        provider: "manual",
        settlements: (payments.data ?? [])
        .filter((item) => item.provider === "manual" && item.status === "succeeded")
        .map((item) => ({ provider_reference: item.provider_reference, amount: item.amount })),
      });
    },
    onSuccess: setReconciliation,
    onError: (error) => message.error(errorMessage(error)),
  });

  const summary = billing.data;
  const period = summary
    ? `${new Date(summary.period_start).toLocaleDateString("zh-CN")} – ${new Date(summary.period_end).toLocaleDateString("zh-CN")}`
    : "本月";

  return (
    <>
      <PageHeader
        eyebrow="Finance"
        title="计费与财务"
        description="用量计量、余额门禁、充值订单、支付退款、双分录账本、发票与对账统一由 Rust 控制面管理。"
        action={<Select allowClear value={projectID || undefined} placeholder="全部项目" style={{ width: 240 }} onChange={(value) => setProjectID(value ?? "")} options={projects.data?.map((item) => ({ label: item.name, value: item.id }))} />}
      />
      <div className="metric-grid">
        <MetricCard icon={Coins} label="本期费用" value={summary ? formatMoney(summary.total.amount, summary.total.currency) : "—"} detail={period} loading={billing.isLoading} />
        <MetricCard icon={WalletCards} label="可用余额" value={account.data ? formatMoney(account.data.balance.amount, account.data.balance.currency) : projectID ? "—" : "选择项目"} detail={account.data?.enforce_balance ? "余额不足时拒绝请求" : "余额门禁未启用"} loading={account.isLoading} tone="blue" />
        <MetricCard icon={MessageSquareText} label="请求数" value={formatNumber(summary?.requests ?? 0)} detail="已入账的推理请求" loading={billing.isLoading} tone="amber" />
        <MetricCard icon={Sigma} label="Tokens" value={formatNumber((summary?.input_tokens ?? 0) + (summary?.output_tokens ?? 0))} detail={`输入 ${formatNumber(summary?.input_tokens ?? 0)} · 输出 ${formatNumber(summary?.output_tokens ?? 0)}`} loading={billing.isLoading} tone="violet" />
      </div>
      {projectID && account.data && (
        <div className="inline-notice billing-policy">
          <Gauge size={18} />
          <span><strong>预付余额门禁</strong> 开启后，余额耗尽的 API Key 会由网关拒绝。</span>
          <Switch checked={account.data.enforce_balance} loading={policy.isPending} onChange={(checked) => policy.mutate(checked)} />
        </div>
      )}
      <section className="panel table-panel billing-workbench">
        <Tabs
          items={[
            {
              key: "usage",
              label: "用量汇总",
              children: <UsageTable loading={billing.isLoading} period={period} data={summary?.projects} />,
            },
            {
              key: "orders",
              label: `充值与退款 (${orders.data?.length ?? 0})`,
              children: (
                <>
                  <div className="table-toolbar">
                    <strong>充值订单</strong>
                    <Button type="primary" icon={<Plus size={15} />} disabled={!project} onClick={() => { topUpForm.setFieldsValue({ amount_yuan: 100, description: "控制台余额充值" }); setTopUpOpen(true); }}>创建充值订单</Button>
                  </div>
                  <Table<BillingOrder>
                    rowKey="id"
                    loading={orders.isLoading}
                    dataSource={orders.data}
                    pagination={{ pageSize: 5, hideOnSinglePage: true }}
                    columns={[
                      { title: "订单", dataIndex: "id", render: (value: string, row) => <div><code className="soft-code">{value}</code><div className="cell-subtitle">{row.description}</div></div> },
                      { title: "项目", dataIndex: "project_id" },
                      { title: "金额", dataIndex: "amount", render: (value: BillingOrder["amount"]) => formatMoney(value.amount, value.currency) },
                      { title: "状态", dataIndex: "status", render: (value: string) => <FinanceStatus value={value} /> },
                      { title: "创建时间", dataIndex: "created_at", render: formatDate },
                      { title: "操作", align: "right", render: (_, row) => row.status === "pending" ? <Popconfirm title="确认这笔支付已到账？" description="初版使用 manual provider；确认后会立即写入双分录。" onConfirm={() => capture.mutate(row)}><Button type="link" loading={capture.isPending}>确认支付</Button></Popconfirm> : null },
                    ]}
                  />
                  <div className="table-toolbar billing-subtable"><strong>支付与退款</strong><Tag>{refunds.data?.length ?? 0} 笔退款</Tag></div>
                  <Table<Payment>
                    rowKey="id"
                    loading={payments.isLoading || refunds.isLoading}
                    dataSource={payments.data}
                    pagination={{ pageSize: 5, hideOnSinglePage: true }}
                    columns={[
                      { title: "支付", dataIndex: "id", render: (value: string, row) => <div><code className="soft-code">{value}</code><div className="cell-subtitle">{row.provider} · {row.provider_reference}</div></div> },
                      { title: "金额", dataIndex: "amount", render: (value: Payment["amount"]) => formatMoney(value.amount, value.currency) },
                      { title: "状态", dataIndex: "status", render: (value: string) => <FinanceStatus value={value} /> },
                      { title: "支付时间", dataIndex: "paid_at", render: formatDate },
                      { title: "操作", align: "right", render: (_, row) => row.status === "succeeded" ? <Button type="link" onClick={() => { refundForm.setFieldsValue({ amount_yuan: row.amount.amount / 100, reason: "客户退款" }); setRefundPayment(row); }}>退款</Button> : null },
                    ]}
                  />
                </>
              ),
            },
            {
              key: "ledger",
              label: `双分录账本 (${ledger.data?.length ?? 0})`,
              children: <LedgerTable loading={ledger.isLoading} data={ledger.data} />,
            },
            {
              key: "invoices",
              label: `发票 (${invoices.data?.length ?? 0})`,
              children: (
                <>
                  <div className="table-toolbar"><strong>发票记录</strong><Button icon={<ReceiptText size={15} />} disabled={!project} loading={issueInvoice.isPending} onClick={() => issueInvoice.mutate()}>开具本月发票</Button></div>
                  <Table
                    rowKey="id"
                    loading={invoices.isLoading}
                    dataSource={invoices.data}
                    locale={{ emptyText: <ResourceEmpty title="暂无发票" description="选择项目后可根据已计费用量开具本月发票记录。" /> }}
                    columns={[
                      { title: "发票", dataIndex: "title", render: (value: string, row) => <div><strong>{value}</strong><div className="cell-subtitle">{row.id}</div></div> },
                      { title: "项目", dataIndex: "project_id" },
                      { title: "金额", dataIndex: "amount", render: (value) => formatMoney(value.amount, value.currency) },
                      { title: "状态", dataIndex: "status", render: (value: string) => <FinanceStatus value={value} /> },
                      { title: "开具时间", dataIndex: "issued_at", render: formatDate },
                    ]}
                  />
                </>
              ),
            },
            {
              key: "reconciliation",
              label: "财务对账",
              children: (
                <div className="reconciliation-panel">
                  <Alert showIcon type="info" message="对账适配器" description="当前 manual provider 使用平台支付记录模拟渠道结算单；正式支付渠道只需提交同一 provider_reference + amount 契约。" />
                  <Button icon={<RefreshCw size={15} />} disabled={!project} loading={runReconciliation.isPending} onClick={() => runReconciliation.mutate()}>执行 manual 渠道对账</Button>
                  {reconciliation && <Descriptions bordered size="small" column={{ xs: 1, sm: 2 }} items={[
                    { key: "provider", label: "渠道", children: reconciliation.provider },
                    { key: "matched", label: "匹配", children: `${reconciliation.matched} 笔` },
                    { key: "platform", label: "仅平台存在", children: reconciliation.platform_only.length || "0" },
                    { key: "provider-only", label: "仅渠道存在", children: reconciliation.provider_only.length || "0" },
                    { key: "mismatch", label: "金额不一致", children: Object.keys(reconciliation.amount_mismatches).length || "0" },
                    { key: "at", label: "执行时间", children: formatDate(reconciliation.generated_at) },
                  ]} />}
                </div>
              ),
            },
          ]}
        />
      </section>
      <Drawer title="创建充值订单" size={480} open={topUpOpen} onClose={() => setTopUpOpen(false)} extra={<Button type="primary" loading={createOrder.isPending} onClick={() => topUpForm.submit()}>创建订单</Button>}>
        <Form form={topUpForm} layout="vertical" onFinish={(values) => createOrder.mutate(values)}>
          <Form.Item label="项目"><Input value={project ? `${project.name} · ${project.id}` : "未选择"} disabled /></Form.Item>
          <Form.Item name="amount_yuan" label="充值金额" rules={[{ required: true, message: "请输入金额" }]}><InputNumber min={0.01} precision={2} prefix="¥" style={{ width: "100%" }} /></Form.Item>
          <Form.Item name="description" label="订单备注"><Input.TextArea rows={3} maxLength={200} /></Form.Item>
        </Form>
      </Drawer>
      <Modal title="发起退款" open={Boolean(refundPayment)} onCancel={() => setRefundPayment(undefined)} onOk={() => refundForm.submit()} confirmLoading={createRefund.isPending} okText="确认退款">
        <Form form={refundForm} layout="vertical" onFinish={(values) => createRefund.mutate(values)}>
          <Form.Item label="原支付"><Input value={refundPayment?.id} disabled /></Form.Item>
          <Form.Item name="amount_yuan" label="退款金额" rules={[{ required: true, message: "请输入退款金额" }]}><InputNumber min={0.01} max={(refundPayment?.amount.amount ?? 0) / 100} precision={2} prefix="¥" style={{ width: "100%" }} /></Form.Item>
          <Form.Item name="reason" label="退款原因" rules={[{ required: true, message: "请输入退款原因" }]}><Input.TextArea rows={3} /></Form.Item>
        </Form>
      </Modal>
    </>
  );
}

function UsageTable({ loading, period, data }: { loading: boolean; period: string; data?: ProjectBilling[] }) {
  return (
    <>
      <div className="table-toolbar"><strong>项目费用明细</strong><Tag>{period}</Tag></div>
      <Table<ProjectBilling>
        rowKey="project_id"
        loading={loading}
        dataSource={data}
        pagination={false}
        locale={{ emptyText: <ResourceEmpty title="本期暂无用量" description="通过网关完成推理请求后，用量会自动进入这里。" /> }}
        columns={[
          { title: "项目", dataIndex: "project_id", render: (value: string) => <code className="soft-code">{value}</code> },
          { title: "请求数", dataIndex: "requests", render: formatNumber },
          { title: "输入 Tokens", dataIndex: "input_tokens", render: formatNumber },
          { title: "输出 Tokens", dataIndex: "output_tokens", render: formatNumber },
          { title: "费用", dataIndex: "cost", align: "right", render: (money: ProjectBilling["cost"]) => <strong>{formatMoney(money.amount, money.currency)}</strong> },
        ]}
      />
    </>
  );
}

function LedgerTable({ loading, data }: { loading: boolean; data?: LedgerTransaction[] }) {
  return (
    <Table<LedgerTransaction>
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
            { title: "账本科目", dataIndex: "ledger_account" },
            { title: "账户", dataIndex: "billing_account_id" },
            { title: "精确金额", dataIndex: "amount_microunits", align: "right", render: (value: number, row) => <span className={value < 0 ? "amount-negative" : "amount-positive"}>{value > 0 ? "+" : ""}{formatMoney(value / 1_000_000, row.currency)}</span> },
          ]}
        />,
      }}
      locale={{ emptyText: <ResourceEmpty title="账本暂无分录" description="用量入账、支付与退款都会生成总和为零的双分录。" /> }}
      columns={[
        { title: "交易", dataIndex: "id", render: (value: string, row) => <div><code className="soft-code">{value}</code><div className="cell-subtitle">{row.description}</div></div> },
        { title: "类型", dataIndex: "kind", render: (value: string) => <FinanceStatus value={value} /> },
        { title: "业务引用", render: (_, row) => `${row.reference_type} · ${row.reference_id}` },
        { title: "分录数", dataIndex: "entries", render: (entries: LedgerEntry[]) => entries.length },
        { title: "时间", dataIndex: "created_at", render: formatDate },
      ]}
    />
  );
}

function FinanceStatus({ value }: { value: string }) {
  const colors: Record<string, string> = {
    succeeded: "success",
    paid: "success",
    issued: "blue",
    pending: "warning",
    refund: "purple",
    top_up: "cyan",
    usage: "gold",
  };
  return <Tag color={colors[value]}>{value}</Tag>;
}
