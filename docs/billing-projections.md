# 事务内计费汇总

增量汇总用于替代高频路径的历史 SUM，不替代原始账本，也不是 Redis 缓存。所有业务持久化通过 SeaORM，迁移 000007 只建立空表和索引，不修改历史记录。

| 表 | 主键 | 内容 |
| --- | --- | --- |
| `xscope.billing_balances` | billing_account_id | 已入账余额、所有 reserved/dispatched 的冻结总额 |
| `xscope.billing_key_holds` | api_key_id | 该 Key 的所有未决冻结额，包括旧月份 |
| `xscope.billing_month_spend` | api_key_id + month | UTC 自然月的用量费用；month 是月初日期 |

## 写入与回填

所有资金写入继续持有账户行锁到 COMMIT。首次找不到汇总时，在同一锁内读取原始数据聚合回填；**不存在不等于余额为零**。之后使用有溢出检查的整数加减：

- reserve：先检查余额/预算，再增加账户与 Key 冻结额。
- dispatch：冻结额不变；未知用量一直保留。
- release：只允许确认未派发的 reserved，减去原冻结额。
- settle：减少完整预占，累计实际月费用、写用量与平衡分录、减少余额、写事件。全部在一个事务内提交。
- 充值、退款和旧 v1 用量都经过同一账本余额更新入口；v1 重放先去重，不重复更新月费用。旧月份的迟到用量按原 `occurred_at` UTC 月份入汇总；v2 使用原有结算入账时间语义，不在本批改变账期定义。
- 零已知用量结算可以释放该笔冻结，但不生成虚构费用分录。未知用量不能走零结算。

回填必须在插入新源记录/改变预占状态**之前**读取历史，避免把本次增量算两遍。账本、用量、预占、汇总和 Outbox 任一步失败均回滚；同一完成请求重试不再次增减。余额允许后付费负数，冻结和花费不允许负数；整数溢出导致事务失败。

Gateway 快照批量读取当前月费用、账户余额和账户配置，不再加载整月用量或逐账户历史分录。已初始化时汇总读取为固定数量的批量查询；未初始化的账户/Key/月仍需锁内回填。快照不是原子跨账户财务快照，仍只是门禁提示，真正资金授权以逐请求预占事务为准。

## 显式校验与重建

仅控制面内部端口 8084，沿用内部 Bearer token，不是 console 用户 API Key。两个接口均锁定目标账户，保证校验时源记录和汇总在该账户下是一致时点：

```text
GET /internal/v1/billing/projects/{project}/projections/check?api_key_id={key}&month=2026-09-01

POST /internal/v1/billing/projects/{project}/projections/rebuild
{"api_key_id":"example-key","month":"2026-09-01"}
```

`month` 必须是月初；Key 必须属于该 project。GET 返回 `stored`、`expected`、`consistent_before`、`rebuilt=false`。尚未初始化的部分为 null，`consistent_before=false` 既可能表示未初始化，也可能表示偏差，应比较 stored/expected 区分。GET 不初始化、不修复、不释放任何预占。

POST 重建整个账户的余额/冻结、所选 Key 的冻结和所选月份的费用，随后在同一事务追加 `projection.rebuilt` 事件（包含重建前后值）。它不修改用量、账本、预占状态或其他月份。回复丢失后重复执行得到相同金额，但会多记一次维护事件；不是具有独立幂等键的审批工作流。重建事件失败也会回滚汇总修改。

这两个接口会扫描相应历史，并暂时阻塞该账户资金写入，**不要当作高频监控调用**。大账户应在维护窗口执行。尚未提供独立维护角色、审批单、供应商证据核验和自动定时修复，内部 token 必须严格限制分发。只有持有完整在线源历史时才能回填/重建；后续归档必须同步设计期初余额和归档基线，不能归档后直接对残余记录 SUM。

## 安全切换版本

旧控制面不会维护这些汇总，禁止与新控制面混跑，禁止直接回滚到旧非投影版本。代码本身不能阻止持有同一数据库凭据的旧进程或手工 SQL 写入；生产迁移仍需独立写入者 fencing/权限与升级流程。

本地使用：

```bash
./tools/bazel-linux.sh images control-plane
bazel run //tools:deploy_billing_protocol
bazel run //tools:billing_protocol_cluster_check
```

部署脚本校验 Docker Desktop / 本地镜像，停止全部现有 control-plane Pods，确认旧写入者退出，再将业务 schema 保存到仓库外权限受限的私有备份目录，然后启动新镜像和验证迁移。完整部署先在控制面可用时排空 Gateway；短暂停机期间新准入失败关闭，不能在控制面停机后才指望内存队列完成上报。不停止数据库、身份服务或其他工作负载，不清空数据。若切换失败，先检查状态和新镜像，**不要自动恢复旧非投影写入者**；脚本可能保留 replicas=0 等待排障。

普通 `tools/deploy-local.sh` 也接入此先停写/备份步骤，再用本地 manifests 恢复控制面。`--reset-business-data` 仍是原本的显式开发重置选项，本次没有使用。当前流程适用于本地短维护窗口，不是生产无停机迁移协议。

## 验证和剩余边界

`bazel run //tools:billing_protocol_smoke` 使用独立 PostgreSQL 和两个控制面/Gateway，检查原始数据与所有已初始化汇总相等、账户并发回填、故障后汇总元组不变、重建事件失败回滚、重复/零/迟到用量、旧月份冻结预算、退款重试和金额溢出。锁住历史表验证热快照仍可完成，锁住账本验证预付费准入不再扫描余额历史。临时数据在结束后清理。

历史增长不再增加已初始化预占路径的 SUM 成本，但账户资金锁仍串行；冷回填也不是常量时间。Gateway 准入单工作线程和 HTTP RTT、有界队列积压、全量 Key 快照大小、源数据保留/归档和消费者吞吐仍需下一阶段处理，不能因此宣称已支持千亿 token/天。
