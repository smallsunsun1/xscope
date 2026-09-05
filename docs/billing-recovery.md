# 计费链路可靠性：WAL checkpoint 与 Redis 配额恢复

这里记录第 5 阶段的 WAL/Redis 可靠性基础。后续已接入 Gateway 资金预占、派发标记与 v2 结算恢复；完整现状和边界见 [资金协议](billing-protocol.md)。仍不代表正式支付或完备的未知用量对账系统。

## 本批已实现

### 持久化用量队列

- 原有 JSONL WAL 保留，仍先写入、fsync，再异步投递；不清空历史记录。旧文件无需格式迁移。
- WAL 本身是队列，不再把待投递事件放进无界内存 backlog。通知只用于唤醒，通知合并或队列满不会丢失待投递事件；重放按文件顺序逐条进行。
- 收到内部用量接口成功响应后，将该记录的起止偏移和 SHA-256 写到 `.checkpoint.tmp`，同步文件、原子 rename、同步目录，才推进内存游标。
- 崩溃后只从 checkpoint 后继续。服务端提交成功但回复丢失时，重复投递原 event_id，继续依赖 PostgreSQL 唯一键及事务防止重复记账。这里是至少一次投递，不宣称端到端 exactly-once。
- 401、403、400、429、5xx 和网络错误均保留当前游标，不再跳过事件。永久拒绝会阻塞后续投递，需要修复凭证/服务端数据契约；日志和 `usage_export/retry` 指标可见。未实现人工跳过或死信审批接口。
- 记录仅持久化 W3C traceparent/tracestate，恢复投递也可保留原 trace 上下文，不写入 Authorization。
- 单个记录上限 1 MiB；内存不加载整份 WAL。独占文件锁禁止两个新 Gateway 进程同时写同一文件。
- 半条尾记录、无效 JSON、checkpoint 与原记录不匹配都不会被静默忽略。错误保留原文件；存储/读取故障使 Gateway readiness 和后续推理请求返回 503。已经在途的请求无法因此获得额外的崩溃保护。

本地 WAL 路径是 `/var/lib/xscope/usage-wal/events.jsonl`，仍挂载原有 256 MiB PVC。checkpoint 是同目录 sidecar。**本批没有压缩、归档或无限容量保证**，需观察磁盘占用；文件填满后不能继续可靠接收推理。

### Redis RPM/TPM 幂等恢复

- 每次服务端预占生成一个 UUID，不使用客户端的 X-Request-Id 作为去重授权。
- 同一分钟桶的同一预占 ID 重试不会再次增加 requests/tokens；同一结算 ID 重试不会重复修正 token。
- 预占/结算重复使用不同 payload 会报冲突。Lua 使用同一个 Redis key，保留 Redis Cluster hash tag；跨 Gateway 共用计数。
- 操作最多两次连接尝试，每次使用同一 ID；连接、读写有超时。接收方已取消的排队命令不会继续启动。
- 结算不会重新创建已过期的桶，也不会无限延长 TTL。未知用量继续保留预估额度直到窗口过期。
- 只重试配额操作，不自动重试模型 POST。RPM/TPM 预占不是资金预占，也不是客户端重试推理的幂等键。

这些保证依赖 Redis 中的幂等状态尚存。Redis 数据丢失、故障转移丢写、请求跨进程重启恢复，以及 Gateway 在使用量最终上报前崩溃的资金保护，仍需要后续正式协议；不能用这次连接恢复测试代替那些保证。

## 构建、部署与复测

```bash
bazel test //...
# 1,100 条遗留 WAL + 8 条在线事件；401、丢回复、强杀重启恢复
bazel test //tools:usage_recovery_test --nocache_test_results
# 两个真实 Gateway + 临时 Redis；已执行但丢回复、重复结算、连接失效
bazel run //tools:quota_recovery_smoke

./tools/bazel-linux.sh images
bazel run //tools:deploy_usage_recovery
bazel run //tools:inference_cluster_smoke
bazel run //tools:telemetry_cluster_smoke
```

quota smoke 使用本地已有的 `redis:8.4.6-alpine` 镜像，创建限额为 0.25 CPU / 96 MiB 的独立 Docker 容器，结束时清理；不会重启或清空集群 Redis。

部署工具仅允许 Docker Desktop 单副本 Gateway。升级前在权限受限的 `.build/usage-backup-*` 中备份当前 WAL，再以 Recreate 策略先停旧 Gateway、后启新 Gateway，避免共享文件的并发写入。**升级期间推理入口会短暂不可用**，控制台和数据库不重启。多副本安装应采用每副本独立持久卷/日志身份，不能直接把 replicas 改为 2 共用这个 WAL。

存储恢复时先停止写入并备份 WAL、checkpoint、临时 sidecar，再调查失败记录；不要直接删除 checkpoint 或截断 WAL。删除 checkpoint 会触发历史重放，绕过坏记录则可能丢失财务证据。

## 第 5 阶段尚未完成

1. 控制面事务协议与 Gateway 准入/新版 WAL 已接通；旧 v1 用量继续兼容上报。资金恢复使用独立 admission WAL，不阻塞在等候同一日志后方的完成记录。
2. 未派发 intent 自动释放，已派发且用量未知保持冻结；供应商证据对账/审批仍待完成，不自动按零费用释放。
3. 与资金事务同提交的 outbox、持久化 pull/ACK 已完成；生产事件消费者和外部发布器尚未接入。参见 [资金协议与测试](billing-protocol.md)。
4. 已补 WAL 本地分段封存、在途预算与磁盘水位门禁；远程归档/压缩、安全回收及多副本独立持久化部署仍待完成。见 [WAL 存储边界](wal-storage.md)。

后续先完成这些后端契约，再进入多集群同步，不提前扩 UI 或宣称正式支付/发票已接入。

实现依赖 Rust 标准库的文件锁/同步、现有 reqwest 和 redis crate；语义参考 [Rust File](https://doc.rust-lang.org/std/fs/struct.File.html) 与 [Redis Lua 原子执行](https://redis.io/docs/latest/develop/programmability/eval-intro/)。
