# Gateway WAL 分段与容量准入

本阶段仍属于计费协议第 5 阶段，不新增服务或 CPU request。两个单写者日志（预占 intent 与最终 usage）共用容量预算，但各自保持独立 checkpoint。业务数据库继续使用 SeaORM。

## 容量保护

| 环境变量 | 默认值 | 含义 |
| --- | --- | --- |
| `XSCOPE_WAL_SEGMENT_BYTES` | 16777216（16 MiB） | 已完整 ACK 的活动文件达到该值后，在下次追加前分段 |
| `XSCOPE_WAL_MAX_BYTES` | 201326592（192 MiB） | 两种日志全部数据（含封存段）与并发预留的逻辑预算 |
| `XSCOPE_WAL_MIN_FREE_BYTES` | 33554432（32 MiB） | 文件系统剩余空间必须保留的安全水位 |

每个已准入请求保守预留 `2 MiB + 32 KiB`：最多各 1 MiB 的 intent/usage，加 checkpoint/分段元数据余量。请求上下文与后台预占命令共享 permit，客户端提前断开或等待超时不会提前释放后台任务的预留。它限制磁盘预算内的并发请求数，并非吞吐量承诺。

使用 `nix::statvfs` 读取文件系统可用空间。超过逻辑预算、低于磁盘水位或空间查询失败时，`/readyz` 和新推理返回 503；`/healthz` 仍用于存活检查。已接收请求的最终用量写入和后台补报继续尝试，不能因为准入关闭就停止结算。空间恢复后容量准入可自动恢复；实际 WAL I/O/校验损坏仍要求修复后重启。

这是应用层保守预算，不是物理预分配，也不是文件系统 quota。其他进程占用空间、inode 耗尽、卷故障仍可能让已接收请求的 WAL 写入失败；此时已派发请求的数据库冻结不会被自动释放。逻辑 retained 统计数据字节，不精确等于包含元数据和块对齐的磁盘占用。PVC 标称容量未必由底层文件系统实施硬配额，所以逻辑上限仍有必要。

## 分段与恢复

以 usage 日志为例（intent 日志同样布局）：

```text
events.jsonl                         原始第 0 段，保留不改写
events.jsonl.checkpoint              第 0 段连续 ACK
events.jsonl.seal                    封存后：代数、字节数、SHA-256
events.jsonl.writer-lock             跨分段的单写者锁
events.jsonl.manifest                当前活动代数
events.jsonl.segments/
  00000000000000000001.jsonl
  00000000000000000001.jsonl.checkpoint
  00000000000000000001.jsonl.seal     该段封存后才存在
  00000000000000000002.jsonl         当前活动段示例
```

只有完整 ACK 的段可以封存。流程是持久化旧段校验信息、创建并持久化空的新文件，再原子替换 manifest，最后允许向新段追加。旧记录、旧 checkpoint 不删除。分段阈值是软阈值：有未 ACK 积压时活动文件可以超过 16 MiB，不能为了轮转跳过待投递记录。

启动时检查封存段 SHA-256、活动文件/checkpoint 和清单。只允许切换前崩溃遗留的下一代空文件；缺失清单却存在后续非空文件、缺失封存段、损坏校验信息、跨代 ACK 均拒绝继续。切换后空活动段可以安全重启。哈希使用固定缓冲区，但启动读取量与全部保留日志大小成正比，不是常数时间启动。

**ACK 不代表财务结清。** 未知用量和超过预占上限的记录在数据库保留冻结、完成待对账投递 ACK 后，也可能进入封存段；这些证据仍全部保留。封存是应用约定及校验保护，并非文件系统不可变属性。

## 运行边界

- 本地继续单副本 Recreate + 原 PVC。多副本必须每副本独立 WAL/PVC，不能共享目录。
- 产生 manifest 后不支持直接回退旧非分段版本；旧版本不认识后续记录。升级过程中保留原文件锁，但不能据此认为停机后的降级安全。
- 当前只有**本地封存**，没有对象存储上传、压缩、自动删除或空间回收。达到上限后应评估扩容并调整预算；不要手工删除已 ACK 段来腾空间。
- 部署工具保留整棵 WAL 目录的私有 tar 副本，并核对更新前各数据文件前缀。运行中复制是取证快照，**不是跨文件/数据库原子恢复点**；tar 检测到并发修改时会在更新前中止。完整恢复仍需停写一致性备份方案。
- 当前版本不宣称已支持一万客户/每日千亿 token 的持续生产负载；容量由请求数、并发、记录大小及持久化延迟共同决定。

## 观测与验证

Prometheus 指标 `xscope_wal_storage_bytes` 的 `kind` 分别取 `retained`、`reserved`、`free`、`limit`、`free_floor`，表达保留数据、在途预算、可用空间、逻辑上限和剩余空间底线。指标在准入/就绪检查时更新。`wal_segment/sealed` 和 `wal_capacity/rejected` 通过现有后台事件计数器输出；后者只记录 reserve 竞争时的拒绝，不等于全部 503 次数。

```sh
bazel test //rust/crates/gateway:gateway_test //tools:gateway_billing_test //tools:usage_recovery_test --jobs=3
```

覆盖单写者、连续 ACK、分段前/后的重启、证据损坏拒绝启动、未 ACK 文件不分段、permit 共享生命周期，以及真实 Gateway 在强制小分段下的 reserve/dispatch/settle 丢 ACK、杀进程恢复。低磁盘水位测试验证 readiness 503、新推理拒绝、已有结算继续重试，恢复水位后没有已 ACK 请求重复投递。真实数据库原子性仍由 `//tools:billing_protocol_smoke` 单独验证。
