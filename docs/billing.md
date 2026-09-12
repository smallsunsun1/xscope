# API Key 策略与财务闭环

当前版本已经形成可验证的计量、预付余额和财务记账闭环。渠道收单和税务开票通过适配器边界接入，当前本地环境使用 `manual` 渠道，不伪装成真实支付。

## 请求路径

1. Rust 控制面将 API Key 的 SHA-256 摘要、Scope、允许模型、过期时间、RPM/TPM、月预算、当月消费和余额门禁发布到内部策略接口。
2. Pingora 网关定期拉取并原子替换内存快照；控制面短暂不可用时继续使用 last-known-good。
3. 网关用 Redis Lua 在单个原子操作中预占 RPM 和估算 TPM；请求结束后按响应中的真实 token 回补差额。所有网关副本共享同一计数窗口。
4. 只有通过策略校验并实际送往模型后端的请求才生成 usage event。资金预占和 dispatch 在推理前同步确认，最终用量由有界内存队列 HTTP 上报；只有确切已知用量才能结算，未知或超限用量保持冻结。
5. 控制面验证租户、项目、Key、模型和价格版本，以 `event_id` 幂等写入 PostgreSQL；用量和对应双分录在同一个 SeaORM 事务中提交，重放不会重复收费。
6. 原始成本以“百万分之一最小货币单位”精确保存；管理台按 UTC 自然月聚合后才转换为分，避免逐请求舍入导致多计费。对外金额仍使用最小货币单位整数。

## 财务对象

- `billing_accounts`：项目币种、可用余额和是否启用预付余额门禁。
- `billing_orders` / `payments` / `refunds`：充值订单、渠道支付和累计退款约束；支付 ID、渠道引用和 HTTP 幂等键防止重复入账。
- `ledger_transactions` / `ledger_entries`：只追加的双分录。每笔用量、充值和退款均生成两条精确微分单位分录，事务内校验合计为零。
- `invoices`：按项目和账期冻结的开票记录；当前不生成税务 PDF，也不调用电子发票平台。
- `reconciliation`：以 `provider_reference + amount` 对比指定租户的渠道结算单，输出平台单边、渠道单边和金额差异。

所有表由 SeaORM entity 与 `sea-orm-migration` 管理。除 SeaQuery 不支持的 `CREATE SCHEMA IF NOT EXISTS` 外，应用不持有裸 PostgreSQL 查询。

## 当前边界

- RPM/TPM 已由 Redis 在多网关副本间原子共享；当前没有独立的并发请求上限，固定分钟窗口也尚未升级为滑动窗口/token bucket。
- 月预算/余额快照是快速门禁，严格资金约束由 PostgreSQL 事务预占和账户行锁提供；Redis RPM/TPM 不替代财务账本。
- Gateway 不再持有本地 WAL/SQLite/PVC。进程强杀可能丢失未确认用量，中央未决记录和双人核查/损失豁免负责收尾，见 [投递边界](usage-delivery.md)。
- 本地 `manual` provider 是可审计的人工确认流程。支付宝 RSA2 支付、回调和退款清算已实现合成签名回归，真实商户接入仍需私有配置与联调，见 [支付边界](payment-providers.md)。
- 发票当前是账期记录，不是税务电子发票；授信、人工调账审批、坏账和总账导出仍未实现。
- 本地策略接口使用集群内服务令牌并由 NetworkPolicy 隔离；多集群生产部署需增加快照签名、版本和成员集群身份认证。
