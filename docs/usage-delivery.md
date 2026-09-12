# 轻量用量投递：HTTP + 有界内存队列

2026-09-06：Gateway 仅支持 `XSCOPE_USAGE_MODE=memory`。它不打开 SQLite、WAL、磁盘容量预算或 OpenDAL 归档，不需要 emptyDir/PVC。接受进程或 Pod 强制终止时丢失未确认用量；**不接受控制面的账本、余额、充值、退款失去事务性**。旧 WAL 写入、checkpoint、回放、分段、磁盘门禁、OpenDAL 归档/回收及其构建依赖均已移除。旧数据不会自动转换或删除。

## 请求与失败语义

1. 准入时为请求预留一个内存槽位，覆盖在途推理、待发送记录和正在发送的 HTTP；拒绝、取消且未产生用量时归还槽位。
2. 资金模式仍同步向控制面执行 reserve/dispatch，确认后才向 Runtime 放行 prompt。所有金额和额度以 PostgreSQL/SeaORM 事务为准。网络不确定时仅尝试账务恢复，绝不重发推理。
3. 请求结束（含 SSE/取消）生成一次不可变最终用量，立即通知后台 HTTP worker。没有每 token 上报，也没有定时累计实际 token checkpoint。
4. 已知且在预占范围内的用量调用 settle。连接错误、408/425/429/5xx 使用相同标识/payload 进行有上限的指数退避（带抖动），**重试次数不设上限**，不改写用量。
5. 其他错误不无限重试、不阻塞后面的记录：保留中央预占并记录 `permanent_failure`。3xx、401、403、404 或本地协议配置错误同时关闭新准入，修复后重启。禁止 HTTP 重定向；3xx 不算成功 ACK。只有 2xx 表示该接口已确认提交。
6. 未知或超限用量调用内部 `.../reservations/{id}/unresolved`，在同一事务内保存结构化待核查说明和事件，预占仍为 `dispatched`。未知 token 用 NULL 表示，不伪造零。重复同一报告不重复写事件，改变报告返回冲突。

丢失 HTTP 响应时可能重复投递，控制面幂等结算保证不重复扣款；这不是网络的 exactly-once。进程强杀时不会从内存恢复记录，中央 reservation 仍可发现。损失上界与在途请求、未 ACK 队列及单请求 token 上限相关，不承诺“只丢几个 token”或某个漏计百分比。

## 默认配置

| 环境变量 | 默认值 | 作用 |
| --- | --- | --- |
| `XSCOPE_USAGE_MODE` | `memory` | 仅兼容值 `memory`；旧 `wal` 或其他值明确拒绝启动 |
| `XSCOPE_USAGE_QUEUE_CAPACITY` | 1024 | 已准入 + 待发送 + 发送中的槽位总数，1–65536 |
| `XSCOPE_USAGE_REPORT_WORKERS` | 4 | 同时发送的 HTTP 上限，1–32 |
| `XSCOPE_USAGE_MAX_PENDING_SECONDS` | 30 | 最老已完成待上报记录达到此年龄后暂停新推理，1–3600 |
| `XSCOPE_GATEWAY_GRACE_SECONDS` | 20 | Pingora 关闭监听后的在途请求宽限期，1–600 |
| `XSCOPE_USAGE_DRAIN_SECONDS` | 10 | 请求运行时关闭后的最终上报排空期限，1–300 |

单事件 JSON 上限 32 KiB，因此不仅限制条数；默认最坏记录载荷约 32 MiB，另有请求上下文、HTTP 缓冲等开销。HTTP 连接超时 2 秒、总超时 3 秒。资金准入仍由单个有界 worker 处理，不把 4 个上报线程等同于生产吞吐认证。

memory 模式必须配置内部 URL/token，即使独立开发关闭了资金预占，也不能悄悄丢弃全部用量。真实地址/凭据由运行时 Secret 注入，不进入构建输入或 Git。

队列满、最老记录超龄、配置故障或进入 drain 时 `/readyz` 与新推理返回 503；`/healthz` 不因此失败，避免 Kubernetes 因积压不断重启并丢数据。已有请求和后台投递继续处理，普通积压恢复后重新准入。凭据/协议故障需要明确修复重启。

SIGTERM 使用 Pingora 自身的关闭通知停止新准入；worker 在在途请求结束期间继续发送。主函数使用 `Server::run` 返回后再限时排空，避免 `run_forever` 直接 `process::exit` 跳过收尾。K8s 默认终止宽限为 60 秒，覆盖 20 秒请求宽限、最多约 10 秒运行时关闭和 10 秒最终排空。调大任一配置时必须同步增加 K8s 期限。SIGKILL、节点损坏和超出期限的长 SSE 不保证排空。

## 未决资金闭环

原来的用量证据提交和双人审批结算仍保留。另新增明确的**损失豁免**，不依赖编造的 provider usage：

- `POST /admin/v1/billing/accounts/{project}/reservations/{id}/waivers`，只有配置的财务审核员可提交，不是客户自助退款。
- 提交字段：`id`、`reason`、`incident_reference`、`request_terminated: true`、`platform_absorbs_loss: true`。审核员须实际核实请求已结束、供应商证据确实不可恢复；布尔声明不是服务端自动验证了供应商状态。
- 在既有 `/reviews/{id}` 查看固定提交及 SHA-256，再由**另一名**财务审核员通过 `/reviews/{id}/decision` 提交 `approve/reject`、准确 SHA-256 和理由。认证禁用模式不能绕过。
- 批准在账户锁内将 reservation/review 转为 `waived`，释放账户及 Key 预占，保存两条不可修改审核记录，产生 `reservation.waived` / `review.waived` 事件。全部同一事务提交，丢响应可幂等重试。
- 实际消费金额仍为 NULL，不生成虚假零 token 用量、充值或双分录。释放的是预占而不是再次增加余额；未知供应商实际成本不作估价入账。
- 适用于 `reserved` 和 `dispatched` 的孤儿请求。普通结算、派发、释放与审批由同一账户锁串行；已豁免后迟到结算返回 409，不再自动扣款。若日后找到真实用量，需另行明确的调整流程，当前不自动反转豁免。
- 仅有超时、查询待核查列表或拒绝审批都不会解冻。历史冻结不由部署工具修改。

## 监控

- `xscope_usage_queue{kind="occupied|pending|oldest_seconds|capacity|ready"}`：总占用、待上报、最老记录年龄、容量和准入状态。
- `xscope_background_events_total`：`usage_export/retry|success|permanent_failure`，`usage_queue/enqueued|rejected|drained|drain_timeout`，`usage_enqueue/error`。
- `xscope_billing_pending_oldest_seconds{state="reserved|dispatched"}`：控制面每 30 秒通过部分索引查询最老中央预占，独立于事件 worker 开关。Gateway 重启丢掉自己的内存指标后，这项监控仍有效；不是精确“丢失 token 数”。
- 新增队列超龄、准入关闭、中央长期冻结告警。标签不含客户 ID/request ID。告警规则不是已配置短信/邮件通知接收器。

## 本地部署与旧数据

新安装或已经运行 memory 模式：`tools/deploy-local.sh`。应用编译、测试和镜像仍全部通过 Bazel（Linux 镜像使用原有 release 构建路径）。默认 Gateway 使用 RollingUpdate，不挂载 usage PVC，不再为新安装创建 usage PVC。普通 apply 不带 prune，因此已经存在的旧 PVC 保留为历史证据；不可用清理脚本或 prune 删除。

完整本地 redeploy 延续原有的财务写入者维护窗口：先在控制面仍可用时停止 Gateway 并等待优雅排空，再停控制面做私有备份/更新，最后恢复期望副本。不先关闭结算接收端，也不在新 Gateway 启动后马上重复 restart。它不是全栈零停机升级；单独更新 Gateway 则使用 Deployment 的 RollingUpdate 策略。

旧集群首次切换时，脚本会在部署变更前拒绝静默忽略旧 WAL：

1. 停止客户端推理流量，保持旧控制面/Gateway 可用，让 reporter 完成 ACK。
2. `tools/deploy-local.sh --accept-volatile-cutover`：先把历史 WAL 私有快照保存在仓库外 0700/0600 目录，校验 checkpoint 尾记录和未确认字节。发现积压、损坏或并发复制失败就中止，不清空数据。
3. 部署期间不要恢复客户端流量；快照是运行中取证副本，不是跨数据库/WAL 的原子恢复点。旧 PVC、对象存储数据及 Secret 均保留，历史未知预占仍等待明确审核。
4. 如有未确认历史记录，保留当前旧镜像和旧写入者，先解决投递再切换。新源码不再提供 `--legacy-wal` 或历史回放工具；不要用新镜像覆盖仍需回放的旧实例。

只保留 `prepare_usage_cutover` 这一只读迁移检查（私有快照、格式与 ACK 校验），不是第二套运行模式。旧 PVC、对象存储数据、归档 Secret 和十年保留约定不由此代码清理修改；历史资产的保留/处置需独立运维决策。本地业务重置脚本也不会再删除旧 usage PVC。

## 验证

```sh
bazel test //tools:usage_memory_test //tools:usage_cutover_test //tools:gateway_snapshot_test //tools:local_secrets_test //tools:streaming_e2e_test
bazel run //tools:billing_protocol_smoke
```

使用真实 Bazel Pingora 进程和故障 HTTP peer 验证 ACK 丢失/SSE、过龄/容量门禁、永久错误/3xx/401、进程强杀不重放推理、SIGTERM 排空和不创建任何 WAL 目录。独立 PostgreSQL 回归验证未决证据幂等、权限、双人豁免、审计失败回滚、并发审批、迟到结算冲突和余额投影一致性；不处理真实客户冻结或联系真实支付渠道。
