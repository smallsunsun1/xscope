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
- 余额/冻结额和 Key 月费用使用同一事务内的增量汇总，缺失时账户锁内回填，初始化后不再逐请求扫描历史；Gateway 快照也批量读汇总。校验/修复、旧写入者排除与归档限制见 [汇总协议](billing-projections.md)，生产容量边界见 [容量分析](billing-capacity.md)。
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
- 本地继续使用单副本 Recreate + 原 PVC，新增一个低开销线程，不增加 Pod/CPU request。多 Gateway 共享资金约束已用真实进程测试，但生产多副本仍需**每副本独立持久卷**；禁止两个副本共享 WAL。已补已 ACK 日志本地分段封存、共享在途空间预算和磁盘水位准入；尚无远程归档、压缩或自动清理。见 [WAL 存储边界](wal-storage.md)。

## 内部接口

前缀 `/internal/v1/billing`，只在控制面内部端口 8084 暴露，沿用内部 Bearer token，**不是 console 用户 API Key**。以下路径中的 project、id、consumer 是调用方提供的标识；成功返回 200。

| 方法、相对路径 | 用途 |
| --- | --- |
| `POST /reservations` | 幂等资金预占 |
| `GET /projects/{project}/reservations/{id}` | 故障后查询持久化状态 |
| `GET /projects/{project}/pending-reservations` | 按年龄与游标分页发现 dispatched 未决预占；只读 |
| `POST /projects/{project}/reservations/{id}/dispatch` | 标记可能已经派发；无业务请求体 |
| `POST /projects/{project}/reservations/{id}/release` | 释放确定未派发的预占 |
| `POST /projects/{project}/reservations/{id}/settle` | 按冻结报价结算已知用量 |
| `POST /projects/{project}/consumers/{consumer}/poll` | 拉取尚未 ACK 的事件，limit 1–100，可携带上一批 ACK |
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

1. `poll {"limit":50}` 返回 data、acknowledged、delivered、has_more、retry_after_ms。未 ACK 时重复 poll 仍返回该批事件。
2. 消费者按序处理并以 `(billing_account_id, sequence)` 去重，再调用 `ack {"expected_sequence":旧ack,"sequence":已连续完成的位置}`。
3. ACK 不得倒退或超过实际投递位置；过期 CAS 返回 409。丢回复后重试同一个已完成 ACK 可返回成功。
4. 控制面重启后进度仍保留。一个 consumer 名称对应一个有序处理者；多个独立订阅使用不同名称。

处理完上一批后，可用一次请求同时 ACK 并获取下一批：

```json
{"limit":100,"ack":{"expected_sequence":0,"sequence":100}}
```

两项进度在一个事务中提交、最多更新一次游标。只能 ACK **之前已投递并连续处理完成** 的事件，不能预先确认下一批。丢回复后重试同一请求不会再次更新已确认的位置；仍返回当前未 ACK 的数据。独立 ACK 接口继续兼容。

已有消费者的空轮询、没有推进 delivered 的重投和重复 ACK 不执行 UPDATE，也不获取行级排他锁。只有进度推进才锁定 `(billing_account_id, consumer)` 这一行，重新读取并验证 CAS；不再锁资金账户。同一账户的不同消费者独立推进。首次注册需要插入游标，可能等待账户外键检查，不能把首次注册说成完全无锁。`updated_at` 表示进度/注册时间，**不再是轮询心跳**。

事件生产者仍从序号分配到提交持有账户锁，避免消费者越过未提交事件。无变更响应可能落后于并发 ACK，这符合至少一次投递；不要让同一个 consumer 的多个进程未经协调地执行非幂等副作用。

空页返回 `retry_after_ms=1000`；这是调用方的最小退避提示，不是服务端自动调度器。消费端应使用带抖动的递增退避（例如 1–30 秒），避免 1 万个闲置账户每秒全量轮询。`has_more=true` 可继续批量追赶；不得先 ACK 再处理事件。

本协议提供至少一次投递。消费者自己的外部副作用仍需幂等处理，不能把数据库事务延伸到外部支付或消息系统。事件表只追加，尚无自动归档/保留期策略；也还不是防管理员篡改的审计存储。

## 未决预占发现

`GET /internal/v1/billing/projects/{project}/pending-reservations?limit=50` 默认列出创建超过 5 分钟、仍为 dispatched 的记录。可指定 RFC3339 `created_before`（不得晚于当前时间）。按 `(created_at,id)` 升序 keyset 分页，返回的 `next` 含 `after_created_at`、`after_id` 和固定的 `created_before`；下一页原样携带这三个参数及 limit，不用 OFFSET。没有下一页时 `next=null`。半截游标或超出 1–100 的 limit 返回 400。

这是等待对账的发现接口，不是供应商证据采集或审批接口。创建时间不等于派发时间；老请求也可能仍在运行。`requires_usage_evidence=true` 提醒调用方：只读查询不会释放、扣款或把未知用量设为零。跨页不是数据库快照，期间已经结算的请求会离开结果；定期重新扫描以发现迟到的状态变化。

迁移 000006 只新增 `(billing_account_id,state,created_at,id)` 索引，不改写历史。当前本地小表使用事务内普通建索引；**生产大表需要单独设计在线建索引/变更窗口**，不能直接把此启动迁移用于海量历史表。

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

# 本轮游标优化及未决查询，只更新控制面
./tools/bazel-linux.sh images control-plane
bazel run //tools:deploy_billing_protocol
bazel run //tools:billing_protocol_cluster_check
```

隔离测试使用已有 postgres:16-alpine 镜像，临时容器限额 0.5 CPU / 192 MiB，结束后清理。覆盖八路跨实例余额/预算竞争、重复和冲突、退款冻结保护、部分取消费用、旧用量入口拒绝双重结算、Outbox 写失败后的全事务回滚、重启后的冻结状态与 ACK 恢复。双 Gateway 测试验证八路并发只允许一个 Runtime 请求，其余七个 402；相同客户端关联 ID 的两次成功请求各自产生一笔平衡分录。

新增测试持有真实账户/消费者行锁，验证读侧隔离、相互不阻塞、未提交事件不可 ACK；验证并发首次注册、ACK CAS、合并拉取的重试、同时间戳的未决分页、鉴权与账户隔离。另建 1 万测试账户/10 万事件执行 1,000 次空轮询，检查所有探测游标的元组版本和时间戳不变。该探测不是生产吞吐认证，详见容量说明。

控制面部署脚本 `deploy_billing_protocol` 现在先停止旧控制面写入者，再把 xscope 业务 schema 备份至权限受限的 `.build/billing-backup-*`，启动新控制面并验证新增表。增量汇总不能与旧非投影版本混跑，这不是普通无停机滚动更新。Gateway 部署脚本复制整棵 WAL 目录（包括分段/checkpoint/manifest/seal）至 `.build/gateway-billing-backup-*`，以 Recreate 更新单写者并校验已有数据前缀。Gateway 的运行中副本不是跨文件原子恢复点。两种切换均会短暂影响新推理准入，但不清空业务、Keycloak、Redis、WAL 或 PVC。

## 下一步

第 5 阶段继续保持未完成：待补未决请求的供应商证据/审批对账、WAL 远程归档与可证明安全的回收、每副本持久卷、生产事件消费者。当前 UI 没有冻结余额/预占管理页面；不提前宣称正式支付、税务发票或完整财务对账已接入。
