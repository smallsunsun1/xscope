# 连续交付验收清单

## 2026-09-12 开发增量（不表示已部署）

新增 [受管入口自动注册、Gateway 许可/ACK、排空与 UID 手动缩容](managed-traffic.md)。状态与证据位于控制面 PostgreSQL，Gateway 不持久化；只覆盖声明独占、使用受管 Envoy 配置的入口。旧入口不自动获得安全保证，KEDA 自动缩容与崩溃实例终止证明仍待接入。下方 2026-09-11 的“自动注册、ACK/drain 待办”由此页细化，不表示原六项规划全部完成。

本轮验证：全仓 26 个 Bazel 测试目标通过；真实控制面/Agent/双 Gateway/临时 PostgreSQL 的受管链路回归通过，Kubernetes 和引擎为模拟服务；固定版本 Envoy 在禁网容器内验证受管配置通过；四个 Rust 服务的 Linux ARM64 release 编译通过。rust-analyzer JSON 已更新。已有 UI 语言切换测试出现一次超时、复跑通过，未修改前端，应继续关注该用例稳定性。工作区脱敏扫描未发现凭据，历史风险仍保留；未部署或重置在线集群。

## 2026-09-11 开发增量（不表示已部署）

新增 [数据库模型目录、多模型路由、价格版本和发布动作](model-catalog.md)。Runtime 的 startup/readiness 路径可配置。新代码继续保留轻量 HTTP 用量投递，不恢复历史 WAL。原六项规划尚未全部完成：自动部署注册、全局 ACK/drain、安全缩容、资源准入及后续生产集成仍有待办；本页下面的“已部署”描述均是历史验收，不代表本次增量已上线。

本轮验证：全仓 25 个 Bazel 测试目标通过；临时 PostgreSQL/双控制面/双 Gateway 回归通过（包含发布历史写入失败时事务回滚）；Gateway、控制面、Operator、Agent 的 Linux ARM64 release 编译通过；Bazel 重新生成 rust-analyzer JSON。工作区脱敏扫描无发现，但不清除既有 Git 历史风险。没有更新在线 Kubernetes，也没有真实 GPU、支付商或第二真实集群验收。

最新变更：[轻量 HTTP 用量投递](usage-delivery.md) 默认替代 Gateway 本地 WAL，增加审计式损失豁免。下面已有部署验收属于历史版本；这次代码、隔离测试和实际集群切换须分别确认，不能据此认定在线 Gateway 已切换。

本次按用户要求连续推进，不以阶段切换作为暂停条件。不把依赖外部授权、配置或实测的项目标为生产完成。

## 已实现、已验证的范围（2026-09-06）

- [x] 用量投递：仅 HTTP + 有界内存重试、SIGTERM 排空、中央未决说明与双人损失豁免。旧 WAL/归档实现及依赖已删除；没有崩溃不丢用量的承诺。
- [x] 事件：任务领取/租约 fencing、批量幂等 effect/ACK、失败退避/死信与受审计重试、积压监控。多控制面故障测试通过，本地 worker 已启用且积压为 0。
- [x] 多集群协议：持久身份/revision/heartbeat/ACK/NACK、轮换撤销、出站 Agent、Kubernetes Lease、资源所有权和 UID/RV 保护。实际 Agent + 控制面 + PostgreSQL 测试通过；K8s 成员 API 为 fixture，第二真实集群未接入。
- [x] 可观测代码：SLO 记录和多窗口燃尽规则、审计意图/完成记录和 append-only DB 保护。历史版本的规则已在本地加载。本地完整推理 trace 有五种服务，十个 Prometheus 目标 UP；通知接收器仍未配置。
- [x] 支付代码：支付宝 RSA2 预下单/查询/回调、金额与商户绑定、幂等充值，退款预扣与有签名成功证据的清算。多进程真实数据库故障回归通过；全部为合成签名，没有真实支付商联调或实际资金流转。
- [x] API：新增独立 scope 的文本 Completions，复用 SSE/取消/HTTP 上报/预占。税务申请持久化为 pending_provider，不冒充税票。
- [x] Bazel 全仓回归、Linux ARM64 release 镜像及本地 Gateway/控制面/Agent 定向更新；运行镜像 ID 已核对。数据库/WAL 私有备份在仓库外保留，无账号/PVC/未知冻结重置。
- [x] 重新生成 Rust Analyzer 项目 JSON；工作区安全扫描无发现，Git 历史仍有三处已知旧凭据记录，发布继续被拦截。

## 未满足的生产验收条件

- 历史对象证据仍按已确认的十年需求保留，不能因删除归档代码而删除云对象、PVC 或更改 WORM。新模式需为 PostgreSQL 财务/事件数据建立独立长期备份和恢复验收。
- 中国大陆/支付宝已确认。需商户签约、私有密钥/公钥配置及公网 HTTPS 回调；未配置时支付能力返回不可用。公钥模式已实现，证书模式尚未支持。
- 税务开票服务商、税目/资质、红冲等规则与沙箱未确定，真实签发关闭。支付宝日账单/手续费/银行到账的正式对账、确定失败退款的解冻协议仍需接入，不能用人工比较或改状态冒充。
- 第二真实成员集群、GPU Runtime/cache-aware 指标及告警通知目标未提供；mTLS、跨地域容灾、自动多模型注册、Embeddings 与生产规模/恢复窗口压测仍不属于已验证完成项。
- 旧凭据应轮换，Git 历史清理需明确授权；不能在存在已知曝光时推送 GitHub。

已有 Go→Rust/kube-rs、SSE、InferencePool/EPP、stable/canary、Operator/KEDA、预占/账本及基础遥测继续按现有回归保护，不在本次重建。

详细契约：[HTTP 投递](usage-delivery.md)、[消费者/SLO/审计](backend-workflows.md)、[多集群](multicluster.md)、[支付/税务](payment-providers.md)。此清单区分代码、隔离回归与真实外部系统验收，不表示全部生产能力已经完成。
