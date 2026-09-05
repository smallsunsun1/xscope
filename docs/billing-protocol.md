# 资金预占协议与事务 Outbox

第 5 阶段由 Rust/Axum + SeaORM 控制面、Pingora Gateway 和 PostgreSQL 实现，不增加常驻服务。Gateway 已实现资金预占准入、派发标记、结算 WAL 与崩溃恢复；K8s Gateway 配置启用 `XSCOPE_BILLING_RESERVATIONS=true`。独立开发进程默认关闭，必须配置控制面内部 URL/token 才能开启。Redis RPM/TPM 仍是独立的流量配额，不等于资金预占。

## 金额和状态

金额继续使用整数 microunits，1 个货币最小单位 = 1,000,000 microunits。预占按照输入/输出 token 上限计算，并把完整模型报价快照存入 reservation；结算使用该快照，不随当前报价变化。当前只支持既有 xscope-demo / CNY 报价。

```text
reserved   ── dispatch ──> dispatched ── 已知用量 settle ──> settled
reserved   ── 确定未派发 release ──> released
dispatched ── 用量未知 / 超过预占上限 ──> 保持冻结，等待对账
```

上图的“用量未知”只适用于 dispatched。Gateway 必须在释放任何 prompt body 给 Runtime 前成功记录 dispatch（连接/HTTP 头可能已发出）。dispatch 重试不会重复写事件，但不能把“已派发”解释为允许再次发送模型 POST。恢复不能证明是否已经发送过的请求，始终保留冻结，不自动重发或退款。

- 预占在项目 billing_account 行锁内计算 `已入账余额 - 活跃预占`。开启 enforce_balance 时不足返回 402；未开启时允许后付费负余额。
- 同一锁保护 API Key 月预算检查：已计量费用 + 尚未解决的预占 + 新预占不能超过预算。旧月份未解决的冻结额也保守计入；不存在基于内存快照的预占竞态。
- 当前余额和冻结额由数据库聚合，不把完整账本/预占列表加载到应用内存。相关账户、Key、请求查询具备索引。
- reservation ID 与完整请求绑定；重试相同 payload 返回同一记录，不同 payload 返回 409。同一个 project/request_id 不能创建第二个 reservation，也不能用于已入库的普通用量。
- release 只允许 `reserved` 且 reason 为 `not_dispatched`。已派发、已结算不能释放；未知用量不能伪装成零费用自动释放。
- settle 只允许 `dispatched`，接受明确已知的 succeeded/cancelled/provider_error 用量；已知的部分取消用量也计费。实际 token 超过预占上限返回 409，冻结额保留，需对账处理。
- 结算将用量、双分录、状态和事件在一个事务中提交；重复同一 settlement 只返回原结果，改变内容返回 409。零已知用量可结算为零，没有虚构的双分录。
- 旧 usage-events 入口拒绝已被 reservation 绑定的 project/request_id，防止同时走两条结算路径。
- 现有账本写入也取得账户行锁；开启余额控制的扣款/退款不能消耗冻结额。关闭资金协议的开发 Gateway 仍是旧路径，不能提供“推理前资金准入”：不足时其用量上报可能等待补款。余额快照门禁不是事务预占的替代品。
- 充值订单和退款对应行使用数据库锁，避免同一订单重复支付和累计退款的并发检查失效。本批没有接入真实支付渠道。

## Gateway 准入、恢复和边界

- 服务端为每次推理生成独立 `req-UUID`，同时作为 reservation ID 和财务 request_id。响应头 `x-xscope-billing-request-id` 可查账；客户端 `X-Request-Id` 仍用于 Envoy/Runtime 日志关联，不能当作免费重试或扣款去重授权。WAL 和原 trace 关联两种 ID。
- 单个有界工作线程先把完整 reserve 请求写入 `events.jsonl.reservations.jsonl` 并 fsync，再 reserve、dispatch、持久化 checkpoint，最后允许转发 prompt。不保存 API Key 明文。排队/网络超时返回 503；明确余额不足返回 402，不触发 Runtime 推理。
- 重启、超时或丢回复后的未确认 intent 只重放账务查询/幂等 reserve。若仍为 reserved，释放确定未派发的预占；若已 dispatched，则保留。恢复过程从不 dispatch 或重发推理。恢复期间 readiness 失败，完成后才能准入新请求。
- 金额使用保守 token 上限：输出默认 1024，必须大于 0 且小于模型上下文；输入预占 `context - output_limit`，不把通用 tokenizer 估计当作财务上界。实际结算只收取已知用量。当前配置上下文为 32768，需与模型目录/Runtime 配置一致；此方式可能冻结明显高于短请求实际费用的金额，后续可引入可信 tokenizer 服务收紧上限。
- Gateway 将输出上限规范化并传给 Runtime，拒绝同时指定两个 max 字段、超长预估输入和 `n != 1`。当前仅支持单结果 chat；扩展多结果/其他推理 API 前需要新的计费边界，不能直接放开。
- 完成后写同一持久卷上的 v2 usage WAL。已知用量走冻结价格 settle；提交后丢 ACK 使用同一 payload 重试。旧 v1 WAL 继续走原 usage-events 接口，不改写或丢弃历史记录。共享 Rust domain DTO 避免两端契约漂移。
- 用量未知或超过预占上限的 v2 记录不作零结算：确认 PostgreSQL 中仍有 dispatched 记录后推进投递 checkpoint，保留原 WAL 证据和数据库冻结，不阻塞后续正常结算。此 ACK 仅表示待对账状态已持久化，**不表示已结算**。日志/`billing_pending` 指标提示人工处理；尚无待对账审批接口或自动估价扣款。
- 存储故障停止新准入。已 dispatch 但尚未写最终 WAL 就崩溃的请求仍有数据库冻结保护，但实际用量可能无法恢复，需供应商证据对账。没有按超时自动释放的定时器。
- 本地继续使用单副本 Recreate + 原 PVC，新增一个低开销线程，不增加 Pod/CPU request。多 Gateway 共享资金约束已用真实进程测试，但生产多副本仍需**每副本独立持久卷**；禁止两个副本共享 WAL。日志暂未分段/压缩，需监控磁盘容量。

## 内部接口

前缀 `/internal/v1/billing`，只在控制面内部端口 8084 暴露，沿用内部 Bearer token，**不是 console 用户 API Key**。以下路径中的 project、id、consumer 是调用方提供的标识；成功返回 200。

| 方法、相对路径 | 用途 |
| --- | --- |
| `POST /reservations` | 幂等资金预占 |
| `GET /projects/{project}/reservations/{id}` | 故障后查询持久化状态 |
| `POST /projects/{project}/reservations/{id}/dispatch` | 标记可能已经派发；无业务请求体 |
| `POST /projects/{project}/reservations/{id}/release` | 释放确定未派发的预占 |
| `POST /projects/{project}/reservations/{id}/settle` | 按冻结报价结算已知用量 |
| `POST /projects/{project}/consumers/{consumer}/poll` | 拉取尚未 ACK 的事件，limit 1–100 |
| `POST /projects/{project}/consumers/{consumer}/ack` | CAS 推进持久化消费位置 |

Reserve 请求示例（测试项目/Key 必须先存在）：

```json
{
  "id": "reservation-example",
  "tenant_id": "tenant-local",
  "project_id": "example-project",
  "api_key_id": "example-key",
  "request_id": "server-generated-request-id",
  "model_id": "xscope-demo",
  "model_revision": "development",
  "price_version": "2026-09-01",
  "input_token_limit": 500,
  "output_token_limit": 1000
}
```

Settle 请求示例：

```json
{"input_tokens":120,"output_tokens":45,"latency_ms":250,"endpoint_id":"demo-pool","region":"local","status":"succeeded"}
```

Release 请求为 `{"reason":"not_dispatched"}`。参数缺失/类型不正确由 JSON extractor 返回 422，业务校验失败 400，余额或预算不足 402，边界不匹配 403，状态/幂等冲突 409。所有请求体拒绝未知字段。恢复原预占/结算不会因 Key 后来撤销而抹掉原有财务义务，但新预占会检查 Key 当前有效性。

## 持久化事件消费

新增 `xscope.billing_reservations`、`xscope.billing_events`、`xscope.billing_consumers`。本批 Outbox 是可重放的 pull 接口，不是 Kafka/NATS 服务，也没有对外 webhook 发布器。

每个账户独立分配 sequence；同一账户从取号到 COMMIT 都持有账户锁。不会使用可能出现提交乱序的全局自增序号作为消费水位。事件包括 reservation.created/dispatched/released/settled 和 ledger.posted。历史账本不自动回填，ledger.posted 覆盖**全部旧控制面实例退出后，由新代码提交的交易**。

消费流程：

1. `poll {"limit":50}` 返回 data、acknowledged、delivered。未 ACK 时重复 poll 仍返回该批事件。
2. 消费者按序处理并以 `(billing_account_id, sequence)` 去重，再调用 `ack {"expected_sequence":旧ack,"sequence":已连续完成的位置}`。
3. ACK 不得倒退或超过实际投递位置；过期 CAS 返回 409。丢回复后重试同一个已完成 ACK 可返回成功。
4. 控制面重启后进度仍保留。一个 consumer 名称对应一个有序处理者；多个独立订阅使用不同名称。

本协议提供至少一次投递。消费者自己的外部副作用仍需幂等处理，不能把数据库事务延伸到外部支付或消息系统。事件表只追加，尚无自动归档/保留期策略；也还不是防管理员篡改的审计存储。

## Bazel 验收与部署

```bash
bazel test //...
# 独立 PostgreSQL + 两个控制面 + 两个 Gateway，不触碰集群数据
bazel run //tools:billing_protocol_smoke
# 真 Gateway + 故障协议服务：丢 ACK / 强杀 / 未知与超限用量
bazel test //tools:gateway_billing_test --nocache_test_results

# 控制面协议已部署后，仅构建并更新 Gateway，不混入其他服务改动
./tools/bazel-linux.sh images gateway
bazel run //tools:deploy_gateway_billing
# 校验真实推理预占 -> 派发 -> 结算及 ledger/outbox 同时落库
bazel run //tools:billing_protocol_cluster_check
bazel run //tools:inference_cluster_smoke
bazel run //tools:telemetry_cluster_smoke
```

隔离测试使用已有 postgres:16-alpine 镜像，临时容器限额 0.5 CPU / 192 MiB，结束后清理。覆盖八路跨实例余额/预算竞争、重复和冲突、退款冻结保护、部分取消费用、旧用量入口拒绝双重结算、Outbox 写失败后的全事务回滚、重启后的冻结状态与 ACK 恢复。双 Gateway 测试验证八路并发只允许一个 Runtime 请求，其余七个 402；相同客户端关联 ID 的两次成功请求各自产生一笔平衡分录。

首次控制面部署脚本 `deploy_billing_protocol` 先把 xscope 业务 schema 备份至权限受限的 `.build/billing-backup-*`，再滚动更新控制面。Gateway 部署脚本备份现有两个 WAL/已有 checkpoints 至 `.build/gateway-billing-backup-*`，以 Recreate 更新单写者并启用开关；推理入口会短暂不可用。两者均不清空业务、Keycloak、Redis、WAL 或 PVC。

## 下一步

第 5 阶段继续保持未完成：待补未决请求的供应商证据/审批对账、分段 WAL/归档和磁盘水位门禁、每副本持久卷、生产事件消费者。当前 UI 没有冻结余额/预占管理页面；不提前宣称正式支付、税务发票或完整财务对账已接入。
