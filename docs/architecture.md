# XScope 架构设计

## 1. 目标与非目标

目标是构建类似 DashScope 的多租户模型服务平台：统一 API、账号与权限、配额和计费、跨模型/跨集群流量调度，以及 Kubernetes 原生部署。首版优先完成一条可审计的端到端链路，不追求一次性实现所有模型和云区域。

非目标：自行实现 GPU 调度器、训练框架或底层推理引擎。它们应通过 K8s、Kueue/Volcano 和 vLLM/Triton/TGI 等后端接入。

## 2. 总体视图

```text
                         ┌──────────── console / SDK ────────────┐
                         │                                        │
                         ▼                                        ▼
                 ┌──────────────┐                        ┌──────────────┐
                 │ Rust Control │                        │ Rust Gateway │
                 │ accounts/RBAC│                        │ auth / quota │
                 │ catalog/bill │                        │ route / proxy│
                 └──────┬───────┘                        └───┬──────┬───┘
                        │ desired state                       │      │ usage
              PostgreSQL│                                    │      ▼
                        ▼                                    │  Kafka/Redpanda
                 ┌──────────────┐                             │      │
                 │ Rust Cluster   │                             │      ▼
                 │ Agent        │                             │ meter/ledger
                 └──────┬───────┘                             │
                        ▼                                     │
                 ┌──────────────┐                             │
                 │ Rust Operator  │                             │
                 │ CRD reconcile│                             │
                 └──────┬───────┘                             │
                        ▼                                     ▼
                K8s workloads ◄──── service discovery ── model backends
                        │                             vLLM/Triton/TGI/custom
                        └──── metrics ──► Prometheus / OTel / logs
```

控制面允许秒级延迟并依赖数据库事务；数据面必须在请求路径上保持无阻塞、尽量无状态。两者只通过版本化配置快照、部署状态和不可变用量事件耦合。

## 3. 逻辑组件

### 数据面

- **Edge/API Gateway**：TLS、OpenAI/DashScope 兼容协议、API Key/JWT、请求大小限制、SSE/WebSocket、错误规范化。
- **Quota & admission**：本地 token bucket + Redis 全局额度；请求前预占、结束后按真实 token 结算。
- **Traffic director**：按 tenant/model/region/版本筛选 endpoint，再结合 readiness、容量、延迟、成本、灰度权重选择；失败仅对幂等且未开始流式输出的请求重试。
- **Usage emitter**：生成全局唯一 `event_id`，记录输入/输出 token、缓存命中、模型版本、租户、价格快照版本。事件投递失败进入本地 WAL/降级队列，不能静默丢失。

### 控制面

- **Identity & tenancy**：用户、组织、项目、服务账号、API Key、RBAC/ABAC、SSO/OIDC。
- **Catalog & pricing**：模型、能力、SKU、上下文长度、区域、价格版本、生效时间；历史订单永远引用旧价格快照。
- **Quota & policy**：RPM/TPM/并发/日额度、允许模型、内容安全策略、地域和数据保留策略。
- **Billing**：预付余额或后付信用额度、用量聚合、双分录账本、账单、支付/退款、发票。账本和原始用量事件应可重放、可对账。
- **Deployment**：模型制品、运行时、GPU 规格、副本和自动扩缩策略，输出 `ModelDeployment` 期望状态。
- **Config distribution**：本地初版通过服务身份认证接口发布 Key 策略，网关原子更新并保留 last-known-good；生产多集群演进为签名、版本化的推送/拉取快照。

### 平台支撑（容易漏掉但必须设计）

- 模型供应链：制品 registry、校验和、SBOM、签名、漏洞扫描、模型许可证和来源审计。
- 安全与合规：密钥轮换、KMS、审计日志、PII 脱敏、内容安全、数据驻留、删除与保留策略。
- 可靠性：多 AZ、灾备 RPO/RTO、熔断/退避、容量保护、幂等、事件去重、对账补偿。
- 可观测性：request/trace ID、RED 指标、token 与首 token 延迟、GPU 指标、SLO 和成本归因。
- 开发者体验：SDK、API 文档、模型 playground、webhook、异步 batch API、错误码和状态页。
- 运营治理：模型审核/下线、灰度、租户封禁、滥用检测、预算告警、工单和人工调账审批。

## 4. 数据与一致性

| 数据 | 建议存储 | 一致性 |
| --- | --- | --- |
| 租户、项目、Key 元数据、价格、账本 | PostgreSQL | 强一致事务 |
| 限流计数、短期配额预占、配置缓存 | Redis Cluster | 原子脚本 + TTL |
| usage/audit/deployment 事件 | Kafka/Redpanda | 至少一次 + 消费端去重 |
| 模型制品、账单归档、请求日志采样 | S3 兼容对象存储 | 不可变/版本化 |
| 指标、日志、链路 | Prometheus + Loki/ClickHouse + OTel | 最终一致 |

关键原则：API Key 只保存哈希；金额不使用浮点数；账本只追加；每个异步消费者以 `event_id` 幂等；价格在请求开始时解析成 `price_version` 并随 usage event 固化。

## 5. 调度算法边界

1. 硬过滤：租户策略、区域、能力、版本、健康、上下文长度、剩余配额。
2. 软打分：`capacity * weight - latency_penalty - error_penalty - cost_penalty`。
3. 一致性选择：会话/前缀缓存场景使用 rendezvous hash；普通请求 power-of-two choices。
4. 过载：先拒绝低优先级，再排队有限时间；永不无限排队。
5. 状态分层：K8s readiness 是慢速事实，主动探测/请求反馈是快速事实；熔断状态仅驻留网关。

## 6. Kubernetes 原生部署

`ModelDeployment` CRD 表达用户意图，Operator 负责 Deployment/Service/HPA/PDB/NetworkPolicy 等派生资源。GPU 资源、节点选择、拓扑分散和优先级都是 spec 的一部分。生产环境建议配合 GitOps；控制面只写 CR，不直接操作 Pod。

租户控制面与推理集群建议分离：管理集群保存 CR 与全局元数据，多个区域推理集群运行网关、Operator 与模型工作负载。跨集群通过声明式同步而非共享 K8s API 权限。

## 7. 代码组织与语言分工

- 根目录：Bazel/Bzlmod 统一构建、测试与后续镜像产出；语言 manifest 和 lockfile 是依赖解析输入。
- `rust/`：Cargo workspace + rules_rust crate_universe；Axum/SeaORM 承担全部业务控制面和 PostgreSQL 访问，Pingora 承担连接池、HTTP 代理、健康检查、鉴权、Redis 配额与 usage。
- `rust/crates/{kubernetes,cluster-agent,operator}`：kube-rs 类型、管理 API 和 kube-runtime 协调器；保持独立集群身份和 RBAC 边界，不承载账号、财务或订单逻辑。项目不再包含 Go module。
- `python/`：pyproject/src layout + rules_python；FastAPI、Pydantic 承担协议校验和开发 runtime。
- TypeScript：Web 控制台与可选 Node SDK。

初期控制面采用模块化单体，共享一个数据库但禁止跨模块直接读表。达到独立扩缩、故障隔离或团队所有权需求后，再沿事件/API 边界拆服务。

## 8. 交付阶段

- **M0（已完成）**：按语言组织的原生工作区、契约、健康检查、CRD。
- **M1（已完成初版）**：PostgreSQL 项目/API Key → 动态策略快照 → Pingora 细粒度鉴权 → FastAPI backend → 持久化 usage WAL → 幂等用量表/费用汇总。
- **M2（已完成初版）**：Operator/cluster-agent、Ant Design 控制台、Redis 全局 RPM/TPM、预付余额、订单/支付退款、双分录、发票记录与对账。
- **M3**：多集群签名期望状态、header 灰度路由、llm-d/KServe KV-aware 调度、自动扩缩、正式支付/税务发票、审计与 SLO。
- **M4**：企业 SSO、数据驻留、batch/fine-tune、市场化模型接入与成本优化。
