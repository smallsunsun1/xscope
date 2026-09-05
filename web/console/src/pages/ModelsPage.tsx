import { useMutation, useQuery } from "@tanstack/react-query";
import { App, Button, Form, InputNumber, Select, Table, Tag } from "antd";
import { Calculator, Cpu, Layers3 } from "lucide-react";
import { api, errorMessage } from "../api";
import { PageHeader, ResourceEmpty } from "../components";
import { formatMoney, formatNumber } from "../format";
import type { Model } from "../types";

type QuoteForm = {
  model: string;
  input_tokens: number;
  output_tokens: number;
};

export function ModelsPage() {
  const { message } = App.useApp();
  const [form] = Form.useForm<QuoteForm>();
  const models = useQuery({ queryKey: ["models"], queryFn: api.models });
  const quote = useMutation({
    mutationFn: (values: QuoteForm) => api.quote(values.model, values.input_tokens, values.output_tokens),
    onError: (error) => message.error(errorMessage(error)),
  });

  return (
    <>
      <PageHeader eyebrow="Catalog" title="模型与价格" description="查看模型上下文限制、当前价格版本，并在调用前估算最高费用。" />
      <div className="models-layout">
        <section className="panel table-panel models-table">
          <Table<Model>
            rowKey="id"
            loading={models.isLoading}
            dataSource={models.data}
            pagination={false}
            locale={{ emptyText: <ResourceEmpty title="模型目录为空" description="向控制面同步模型和价格后会显示在这里。" /> }}
            columns={[
              { title: "模型", dataIndex: "display_name", render: (name: string, record) => <div className="primary-cell"><span className="table-icon blue"><Layers3 size={16} /></span><span><strong>{name}</strong><small>{record.id}</small></span></div> },
              { title: "上下文", dataIndex: "max_context_tokens", render: (value: number) => `${formatNumber(value)} tokens` },
              { title: "输入 / 1M", dataIndex: "input_per_million_tokens", render: (money: Model["input_per_million_tokens"]) => formatMoney(money.amount, money.currency) },
              { title: "输出 / 1M", dataIndex: "output_per_million_tokens", render: (money: Model["output_per_million_tokens"]) => formatMoney(money.amount, money.currency) },
              { title: "价格版本", dataIndex: "price_version", render: (value: string) => <Tag>{value}</Tag> },
            ]}
          />
        </section>
        <aside className="panel quote-panel">
          <div className="quote-icon"><Calculator size={22} /></div>
          <span className="panel-kicker">Estimator</span>
          <h2>费用估算</h2>
          <p>结果按当前价格版本计算，并向上取整到最小货币单位。</p>
          <Form form={form} layout="vertical" initialValues={{ input_tokens: 1000, output_tokens: 500 }} onFinish={(values) => quote.mutate(values)}>
            <Form.Item name="model" label="模型" rules={[{ required: true, message: "请选择模型" }]}><Select placeholder="选择模型" options={models.data?.map((model) => ({ label: model.display_name, value: model.id }))} /></Form.Item>
            <div className="form-pair">
              <Form.Item name="input_tokens" label="输入 tokens" rules={[{ required: true }]}><InputNumber min={0} precision={0} /></Form.Item>
              <Form.Item name="output_tokens" label="输出 tokens" rules={[{ required: true }]}><InputNumber min={0} precision={0} /></Form.Item>
            </div>
            <Button type="primary" htmlType="submit" block loading={quote.isPending} icon={<Cpu size={16} />}>计算最高费用</Button>
          </Form>
          {quote.data && <div className="quote-result"><span>预计最高费用</span><strong>{formatMoney(quote.data.maximum.amount, quote.data.maximum.currency)}</strong><small>价格版本 {quote.data.price_version}</small></div>}
        </aside>
      </div>
    </>
  );
}
