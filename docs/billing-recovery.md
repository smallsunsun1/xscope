# 计费链路可靠性：HTTP 上报与 Redis 配额恢复

Gateway 本地 WAL 已移除。唯一投递路径为 [HTTP + 有界内存队列](usage-delivery.md)；中央资金预占、幂等结算、事务事件和双人核查见 [资金协议](billing-protocol.md)。

## 故障边界

请求完成后立即触发后台上报；暂时 HTTP 故障按原事件身份重试，提交后丢 ACK 不重复扣款。队列容量/年龄门禁避免无限积压，SIGTERM 限时排空。SIGKILL、Pod 替换、节点损坏仍可能丢失尚未确认的实际用量；中央 hold 保留，并由索引化未决监控发现。没有“超时即免费”或自动重发推理。

已知证据可经双人审批幂等补结算；确实无法取证时，只能由独立审核者批准平台承担损失的豁免。未知费用保持 NULL，不用假零结算冒充对账。

### Redis RPM/TPM 幂等恢复

- 每次服务端预占生成一个 UUID，不使用客户端的 X-Request-Id 作为去重授权。
- 同一分钟桶的同一预占 ID 重试不会再次增加 requests/tokens；同一结算 ID 重试不会重复修正 token。
- 预占/结算重复使用不同 payload 会报冲突。Lua 使用同一个 Redis key，保留 Redis Cluster hash tag；跨 Gateway 共用计数。
- 操作最多两次连接尝试，每次使用同一 ID；连接、读写有超时。接收方已取消的排队命令不会继续启动。
- 结算不会重新创建已过期的桶，也不会无限延长 TTL。未知用量继续保留预估额度直到窗口过期。
- 只重试配额操作，不自动重试模型 POST。RPM/TPM 预占不是资金预占，也不是客户端重试推理的幂等键。

这些保证依赖 Redis 中幂等状态尚存。Redis 丢写/故障转移与生产吞吐需单独验证；Redis 额度不是财务账本。Gateway 丢失内存记录不会删除 PostgreSQL 已提交的预占或账本。

## 验证

```sh
bazel test //tools:usage_memory_test //tools:streaming_e2e_test
bazel run //tools:quota_recovery_smoke
bazel run //tools:billing_protocol_smoke
```

quota smoke 使用独立、限额 0.25 CPU / 96 MiB 的 Redis 容器及两个真实 Gateway，验证配额丢 ACK 重试、重复冲突、断线重连和 HTTP 用量总数一致；不修改集群 Redis。

## 旧版本迁移

按 [部署和旧数据步骤](usage-delivery.md#本地部署与旧数据) 停止流量、等待旧 reporter ACK、执行仓库外只读快照检查后切换。新源码没有 WAL 回放器；有积压时保留旧镜像解决投递，不截断文件、不删除 checkpoint/PVC/对象存储证据。PostgreSQL 备份、事件归档和长期财务保留不能因 Gateway 无状态而省略。
