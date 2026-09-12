# 后台消费者、审计与 SLO

最新用量路径和豁免例外见 [轻量 HTTP 投递](usage-delivery.md)。消费者已支持 `reservation.unresolved`、`reservation.waived` 和 `review.waived` 来源验证；它们不会触发额外扣款。新增内存队列及中央长期预占告警。

## 生产事件消费协议

控制面内置消费者由运行时 Secret 中 `XSCOPE_EVENT_WORKER_ENABLED=true` 启用，无需为本地开发新增服务或 CPU request。`tools/deploy-local.sh` / 定向控制面更新会保留或创建 `xscope-backend-runtime`；已有 Secret 不覆写。

- 固定消费者 `ledger.audit.v1`；当前工作是验证账本/预占/核查事件来源并建立幂等处理证明，不是外部支付执行器，也不会自动释放未知用量冻结。
- 事件与任务 high-watermark 同一财务事务提交。多副本用 `FOR UPDATE SKIP LOCKED` 领取 account-scoped 批次，任务不与另一 worker 重复拥有；同账户仍有序。
- 租约使用数据库时钟，ACK 绑定 epoch/token/through/expected position。批次最多 100 条，处理证明批量写入，ACK 与 effect 原子提交；临近提交再次检查租约是否过期。重复完成不重复产生效果。
- 崩溃/显式失败进入 2–256 秒退避，八次失败进入 dead。owner 的 `POST /admin/v1/billing/accounts/{project}/event-worker/retry` 必须提供有界 reason，记录审计；它不跳过 ACK 或删源事件。
- `GET .../event-worker` 返回有界状态，不返回 lease token。空轮询和重复 ACK 无位置写入。未识别的财务事件保留原始事件并失败，不猜测其业务含义。
- `xscope_event_worker_state{state,kind}` 的 jobs/backlog/oldest_seconds 为共享数据库视图；多控制面副本取 max，而不是 sum。请求/失败由 `xscope_background_events_total` 计数，label 不含客户 ID。

短时 1 万账户/10 万事件探针用于验证只读空轮询，不代表千亿 token/天的生产容量。当前每个控制面数据库连接池上限四，消费者单循环；账号热点、数据库 IOPS、长期表增长和故障恢复窗口仍需真实负载压测。

## 审计

Console 修改操作先持久化 `console.intent`，完成后记录 `console.completed`。审计只记录 actor、匹配路由模板、方法、状态、intent ID，不复制请求体、Cookie、Token 或 URL query。若操作已提交但完成审计写失败，保留 intent 并发出告警，不谎称操作已回滚。

`GET /admin/v1/audit?limit=100&after=...` 是平台管理员专用 keyset API。数据库 trigger 禁止对 operation_audits、billing_effects、cluster_revisions、provider_receipts UPDATE/DELETE/TRUNCATE；数据库超级管理员仍有能力绕过，不宣称防 DBA 篡改。金融核查证据沿用独立双人审批与不可变审计。

## SLO

规则在 `deploy/k8s/observability/alerts.yaml`。初始运行目标为 99.9% 可用性/30 天，95% 首字节不超过五秒；它们不是商业 SLA，首字节也不是模型 token-aware TTFT。

- 5xx 或 provider_error（包括 HTTP 200 的错误 SSE）计入错误；主动客户端取消与鉴权/预算等 4xx 不进入分母。无流量保留无样本/NaN，不伪装为百分之百健康。
- 记录 5m/30m/1h/6h/30d 错误率、30 天剩余错误预算和慢首字节比例。
- 14.4 倍的 1h+5m 快速燃尽、6 倍的 6h+30m 慢速燃尽，以及 HTTP 投递/消费者死信与积压/审计缺口/成员 NACK 告警。
- `bazel run //tools:slo_rules_check` 使用实际 Prometheus promtool 验证健康、无样本、错误率、窗口和取消/4xx 排除。`bazel run //tools:deploy_observability_alerts` 只更新规则引用，保留 live scrape、TSDB 和 Grafana。
- 告警通知接收目标尚未配置，规则加载不等于已发送短信/邮件/飞书通知。

## 推理 API

除原有 Chat Completions 外增加 `POST /v1/completions`，API key 必须有独立 `completions` scope；复用同一已注册模型的 RoutePolicy / Pool、SSE、取消、HTTP 用量上报、RPM/TPM 与金额预占。当前仅一条非空字符串 prompt，不支持 prompt batch/token ID arrays，财务协议只允许 n=1。有完整 provider usage 才能结算，无证据继续冻结。原有静态回退 key 仍只授权 chat，不隐式扩大权限。

本地 Echo Runtime 不宣称具备真实文本生成或 Embeddings 能力。Embeddings、工具调用专门计价、多模型自动注册仍需对应 Runtime/模型能力与计量契约，未作为这次文本补全入口的已实现能力发布。
