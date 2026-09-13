# 可靠性与管理闭环（2026-09-13）

本轮继续第 2、3 组工作：计费并发、业务库备份恢复、站内告警、成员断网恢复回归，以及已稳定后端契约的管理页面。跳过真实 GPU 验收与支付宝接入，不把本地 Echo、合成故障或独立 PVC 宣称为商用生产验收。

## 计费并发与停止行为

Gateway 仍然只有有界内存队列和幂等 HTTP 用量投递，没有恢复 SQLite、Gateway WAL 或归档路径。

| 运行时配置 | 默认 | 校验范围 |
| --- | --- | --- |
| `XSCOPE_BILLING_ADMISSION_WORKERS` | 4 | 1–32 |
| `XSCOPE_BILLING_ADMISSION_CAPACITY` | 64 | 1–4096，等待队列容量 |
| `XSCOPE_BILLING_ADMISSION_TIMEOUT_MS` | 8000 | 100–60000 |
| `XSCOPE_DB_MAX_CONNECTIONS` | 4 | 1–128，每控制面进程 |
| `XSCOPE_DB_MIN_CONNECTIONS` | 0 | 0–max |
| `XSCOPE_DB_ACQUIRE_TIMEOUT_MS` | 2000 | 100–30000 |

实际准入占用上限为等待容量加 worker 数，同时仍受 Gateway 用量队列总槽位约束。工作线程只共享短暂出队锁，不在 HTTP 调用期间持有该锁。到期或取消的排队请求不会继续 reserve；已 reserve 后取消会尝试恢复，已 dispatch 的不确定请求继续冻结，不重放推理。

`xscope_billing_admission{kind}` 暴露 occupied、active、workers、queue_capacity；`xscope_billing_admission_duration_seconds{phase="queue|protocol"}` 区分排队和 reserve/dispatch 时间。拒绝与超时进入 `xscope_background_events_total{operation="billing_admit"}`。标签不包含客户或请求 ID。

控制面数据库连接获取有超时，空闲连接和连接寿命有界；默认仍为四连接，不为本地开发默认扩大 CPU/连接配额。跨副本预算应计算所有控制面连接数、备份和运维连接。热账户资金锁没有被绕开。

控制面处理 SIGTERM/SIGINT，并停止两个 HTTP listener 接收新请求；允许已接收的内部请求完成后再停止 worker，内部排空另有 20 秒等待上限。Gateway 的用量丢失边界不因此改变。

### 压力回归

```bash
# 默认每种分布 128 个完整 reserve → dispatch → settle；20% 重放 settle。
bazel run //tools:billing_protocol_smoke -- --load-only

# 运行时参数，不把私有凭据放入 Bazel action 环境。
XSCOPE_LOAD_REQUESTS=1000 XSCOPE_LOAD_CONCURRENCY=16 \
  bazel run //tools:billing_protocol_smoke -- --load-only
```

范围：两个真实控制面和独立 PostgreSQL（0.5 CPU、192 MiB），不含 Gateway/Envoy/EPP/真实推理。默认短测最后一轮：16 个账户约 78.48 个完整请求/秒，P95 207.30ms；单热账户约 43.74 个/秒，P95 193.26ms。更早同机一轮为 65.82 / 38.21 个/秒，显示资源竞争和短测方差明显。账本平衡、投影一致、无遗留 hold、重复 settle 不重复扣费均检查通过。

这不是容量上限或优化倍数，也不是每天千亿 token 的认证。持续长稳态、真实峰谷、VACUUM、连接饱和和积压恢复仍需真实业务负载验证。此入口还检查 SIGTERM 时已接收的内部 HTTP body 能完成，而不是立刻被 abort。

## 业务库备份和恢复

使用 PostgreSQL 自带的 `pg_dump` / `pg_restore`，通过一个保持打开的导出快照连接，将 `xscope` schema、`public.seaql_migrations` 和逐表计数放在同一数据库快照。两份 custom archive 分开保存，避免把公共 schema 中的身份数据一起备份；业务函数、触发器和约束随 schema 导出。[PostgreSQL 备份工具说明](https://www.postgresql.org/docs/16/app-pgdump.html)。

- 每天北京时间 02:15 执行 CronJob，禁止计划任务重叠；任务有 900 秒截止时间和有限重试。
- 专用只读数据库身份只获得业务 schema 与迁移表读取权限。实际连接信息和凭据通过 Secret 注入；Job 没有 Kubernetes API token。
- 独立 10Gi PVC 保存归档与 SHA-256 校验清单。COMPLETE 标记最后写入；不完整目录不能进入恢复候选。
- 不自动删除归档，包括不完整目录；不更改此前确认的历史十年保留需求，不操作旧 Gateway PVC/云对象，不创建不可撤销的 WORM 策略。磁盘容量需要扩容或另行制定经验证的中央备份归档策略。
- 恢复演练先检查校验和，再在禁网、无宿主端口、一次性的 PostgreSQL 容器中恢复，检查所有表计数、账本平衡、审计保护和身份表未被导出。
- 私有临时文件位于仓库外 0700 目录、0600 文件中；演练结束移除临时副本，PVC 备份保留。恢复不指向在线数据库。

这是每日逻辑快照，**不是异地容灾、持续 PITR 或已经满足十年合规归档**。同一集群/PVC 故障仍可同时损失主库和备份；也未备份 Keycloak 身份、Kubernetes Secrets、Redis 等整个平台状态。恢复演练通过不等于已证明整个系统 RPO/RTO。[恢复工具说明](https://www.postgresql.org/docs/16/app-pgrestore.html)。

```bash
bazel run //tools:reliability_local -- backup
bazel run //tools:reliability_local -- restore-drill
# 只清理本工具已完成的临时 Job/Pod，不删备份、失败 Job 或 PVC。
bazel run //tools:reliability_local -- cleanup
```

本地真实业务库演练通过，所有快照表计数一致、账本平衡、审计触发器保留，未包含身份表。测试发现同容器快照客户端的信号清理会触发 PostgreSQL 重初始化；改为通过私有 FIFO 发送 ROLLBACK/退出后，五路并行隔离恢复测试全部通过。计划 Job 使用相同的修正脚本。

## 站内告警与备份监控

Prometheus → Alertmanager → 独立凭据保护的控制面 webhook → SeaORM/PostgreSQL 告警中心。复用 Alertmanager 的投递和重试，不另写通知调度器。当前低资源安装按完整 label-set 分组，接收端限制 100 条/256KiB，拒绝截断批次。[Alertmanager 配置说明](https://github.com/prometheus/alertmanager/blob/v0.28.1/docs/configuration.md)。

接收器对 fingerprint 和 startsAt 去重，resolved 不被迟到 firing 撤销；重复投递保留已有人工确认。人工确认记录操作审计，但不等于解决告警，也不影响任何资金冻结。UI 只向平台管理员开放。原始标签、链接和凭据不会直接作为页面动作执行。

Alertmanager 使用独立 PVC；精简 kube-state-metrics 只读取当前 namespace 的 CronJob、Job、PVC。新增两个常驻进程合计 CPU request 20m，备份任务执行时额外 10m。新规则覆盖准入拥塞、通知失败、备份失败、超过 26 小时没有计划成功以及监控不可用。外部短信/邮件/飞书未配置；整个监控或控制面故障时，不能把站内通知当成独立的带外告警渠道。

```bash
bazel run //tools:reliability_local -- alerts
bazel run //tools:reliability_check
bazel run //tools:slo_rules_check
```

实际本地告警触发与恢复均已到达中央通知表；匿名 API 被拒绝，备份只读角色不能读身份表，两个新增监控目标 UP，规则正常加载。规则测试包括无计划成功、最近成功、失败任务与原有 SLO。

## UI 和权限

控制台新增“运行与恢复”：受管池及在途会话、自动扩缩操作、成员 heartbeat/desired/ACK、告警中心、备份计划观测、分页操作审计。未观测到就绪显示未知；retired 会话的 -1 在途显示未知，不变成零。自动缩容进行中禁止手动动作，操作使用 expected_generation，服务端继续复核证据。

“流量路由”增加发布历史、暂停、提升、回滚、已知会话 ACK；版本冲突后要求重开，不自动用新版本覆盖别人的修改。“用量与计费”增加证据提交、独立审批、明确损失豁免、审计以及死信重试。

- 平台管理员：受管池、成员集群状态、运行告警和操作审计。租户 owner 不自动获得这些权限。
- 租户 owner：提交本项目用量证据、管理发布、查看/重试事件任务。
- 显式授权的财务审核员：审批和申请损失豁免；不能审批自己的提交。页面项目选择仍使用该账号可见的项目列表。
- 没有增加账号或扩大已有人员权限；没有独立审核员时，审批按钮不可用，冻结继续保留。

首轮部署时两份授权名单均为空。后续按用户明确确认，已将其指定现有账号对应的 OIDC subject 加入平台管理员名单；平台数据库与 Keycloak 身份交叉核验通过，并在新的就绪控制面 Pod 中验证配置已加载。当前平台管理员 1 位，财务审核员仍为 0，财务审批保持受限；没有因为用户名包含 admin 自动升权。实际 subject 只写入 Secret，不进入仓库。

`bazel run //tools:reliability_local -- auth-status` 只输出名单人数。显式授权操作 `grant-platform-admin` 从 stdin 接收单个 `username` 的 JSON，要求唯一有效且平台/Keycloak 匹配的身份，幂等追加，不改变财务审核员名单。验证跳过滚动更新中正在终止的旧 Pod；不伪造 OIDC 请求头来代替正常登录。

只读成员集群页面没有提供新集群注册/凭据轮换/任意 desired-state 编辑。备份恢复也没有新增绕过 Kubernetes 运维权限的公共写接口。告警中心不是完整故障工单系统。

19 个浏览器用例通过，覆盖双语、权限、代次/版本冲突、未知在途、原始证据必填、独立审核员审批和明确告警确认。真实浏览器访问已到达 OIDC 登录页，但当前会话未登录，因此未将在线登录后的点击操作宣称为本轮已验证。页面静态资源随 release 控制面镜像部署。

## 本地增量升级

已有安装建议用定向升级，保留 live Secret、受管入口及观测配置；不是重新应用基础 overlay：

```bash
XSCOPE_BUILD_ARCH=arm64 bash tools/bazel-linux.sh images control-plane gateway
bazel run //tools:reliability_local -- upgrade
```

升级先备份，后排空 Gateway，再更新控制面及 Gateway，有短暂维护窗口。备份失败时不停止服务。中途失败保留原副本数；修复构建/部署问题后用 `resume-upgrade` 恢复已备份操作，不用重新抓取零副本覆盖原值。基础 `deploy-local.sh` 仍是基础栈部署入口，不代表自动重新配置这些运维扩展。

本轮没有真实第二集群。新故障回归在实际 Agent/控制面/PostgreSQL 之间注入链路中断，检查未应用 revision 不 ACK、重连后 UID/RV 更新；Kubernetes API 仍为 fixture。这不替代跨地域、mTLS、云主机隔离或整个平台恢复验收。

最终 19 个 Deployment 就绪，原五个 PVC 与新增的通知/业务备份 PVC 全部 Bound。清理的只是六个本工具创建的已完成临时备份 Job 及其 Pod，所有备份文件保留。最新脱敏扫描在工作区及可达 Git 历史均未发现匹配，不证明聊天中披露的旧凭据已经失效；本轮未轮换凭据、重写历史或推送仓库。
