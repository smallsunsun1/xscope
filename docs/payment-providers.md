# 支付与税务边界

业务数据库使用 SeaORM；RSA2 使用 AWS-LC、PEM/表单和 HTTP 使用现成库。优先中国大陆 CNY / 支付宝，按整数分计算，拒绝浮点舍入、科学计数法和超出边界的金额。

## 支付宝

- 私有 `XSCOPE_ALIPAY_CONFIG_FILE` 字段：`app_id`、`seller_id`、`environment`（production/sandbox）、`notify_url`、`private_key_file`、`alipay_public_key_file`。配置和密钥经 Secret 只读挂载；私钥为 PKCS8，公钥为 SPKI/PKCS1，RSA2 公钥模式。不支持支付宝证书模式，不能把证书文件冒充公钥。
- 接口：订单 `POST /admin/v1/billing/orders/{id}/alipay/checkout` 先持久化商户/环境绑定再预下单；`.../sync` 查询支付宝并验证原始 JSON 签名。超时是未知结果，不是付款失败。
- 公共 `POST /payments/alipay/notify`（以及 `/api` 前缀）限 64 KiB；拒绝重复表单字段、商户/算法/金额不匹配，先验签再锁订单。付款、平衡分录、余额投影、outbox、原始签名证据、订单状态同一事务提交；提交成功才返回 `success`。重放不重复充值。
- sandbox 只保存观察证据，绝不增加可消费余额。生产商户凭据尚未配置；无任何真实充值或退款验证。`GET /admin/v1/billing/capabilities` 如实返回配置/能力状态。
- 人工付款入口只允许明确的 `manual-test`、禁用 console auth 且没有支付宝配置的开发实例。正式鉴权实例禁止人工伪造支付成功；不能用通用退款入口修改支付宝付款。
- 生产需要公网可达 HTTPS 回调入口、独立精确回调路由（不要求 OIDC 登录）、NetworkPolicy/限流及支付宝商户签约。尚未修改本地 OAuth2 Proxy 绕过规则，也没有公开内网端口或真实商户配置。

## 退款与补偿

`POST /admin/v1/billing/alipay/refunds` 接收既有 CreateRefundRequest。固定退款 ID（不超过 64 字符）是支付商 `out_request_no`，重试不能更换。锁付款、校验累计退款与可用余额，先把客户余额转到 `refund_pending`；即使推理为后付费也不能透支退款。

`POST .../refunds/{id}/submit` 发起同 ID 的支付宝退款；成功提交后至少间隔十秒再调用 `.../sync`。只有验签通过且精确匹配订单、交易号、退款 ID、金额的 `REFUND_SUCCESS` 才把 pending clearing 转到 cash clearing，不再次扣客户余额。超时/失败/不明确状态继续扣留资金，没有自动解冻接口。

故障恢复可通过 owner-only `POST .../refunds/{id}/evidence` 上传原始已签名的支付宝退款查询 JSON（最多 256 KiB）。依然经过同样验签和精确绑定；上传一段自称成功的 JSON 无效。原始证据、清算分录与退款终态一起提交，重复确认只返回已有结果。缺少成功证据的失败退款解冻，需要后续有确定失败凭据的审查协议，不靠人工改状态。

原有 `/billing/reconciliation` 是上传结算列表的比较工具，不是支付宝日账单拉取、验真、手续费/银行到账的正式对账；目前未宣称正式财务对账已接通。

## 税务申请

原有 invoices 只是用量账单，新记录状态为 `statement_ready`；历史 `issued` 字段也不代表法律意义的税票。`POST /admin/v1/billing/invoices/{id}/tax-request` 收集标题及可选纳税人识别号，数据库幂等保存；GET 不回显纳税人身份。当前数据库状态只允许 `pending_provider`，明确返回 `tax_invoice_issued:false`。此申请不证明可开票金额/项目/税率，也不发送至任何开票机构。

开票服务商、资质、税目、红冲/作废和沙箱尚未确定，因此税务签发能力关闭。账单、支付回单、税票三者不可互换。

## 验证

`bazel test //rust/crates/payments:payments_test` 验证 RSA2、原始 JSON 签名、退款绑定和整数金额。`bazel run //tools:billing_protocol_smoke` 用仓库外即时生成的 RSA 密钥，运行多个真实控制面进程与隔离 PostgreSQL，覆盖并发充值、回滚、沙箱隔离、退款预扣/有证据完成和幂等申请。测试不调用支付宝、不会动真实资金，也不能替代真实商户沙箱联调。

协议依据：[支付宝官方 SDK](https://github.com/alipay/alipay-sdk-java-all)、[官方退款查询字段与延迟说明](https://github.com/alipay/alipay-sdk-java-all/blob/master/v2/src/main/java/com/alipay/api/response/AlipayTradeFastpayRefundQueryResponse.java)。
