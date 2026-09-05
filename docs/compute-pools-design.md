# RFC：算力资源池、组织配额与弹性共享

状态：方案方向已确认，分阶段实施；日期：2026-09-05。

评审补充（2026-09-05）：将基于流量、TTFT/TPOT 的 GPU 自动扩缩容列为标准 LLM 部署的核心合同，详细设计见 [LLM SLO 自动扩缩容](llm-autoscaling-design.md)，而非只保留 CPU HPA 或将其视为可选优化。

本文是目标设计，不是已交付能力清单。用户已确认采用 KEDA 作为弹性配置入口，第一批从 Operator/KEDA 基础链路开始，见 [实现范围](keda-autoscaling.md)。本文 ComputePool/PoolGrant 等新字段和 API 仍未实现，不能直接提交给当前平台。

## 1. 建议结论与评审重点

建议采用：**共享物理算力池 + 团队/项目分层授权配额 + 少量不可出借的在线保障容量 + 可借用、可回收的弹性容量**。

团队和项目可以申请使用不同的池子，管理员也可以直接分配；分配的是“在哪个池上，以什么额度和保障等级使用资源”，不是默认给每个团队划走一批机器。这样能保留业务边界，同时减少资源碎片和闲置。

本设计要求区分三件事：有权使用、已获得资源准入、服务已经就绪。管理员批准 4 张 GPU 的额度，不等于 4 张卡已经空闲，更不等于模型已经加载完成。

建议优先 review 以下决策；后文给出具体语义和验收条件。

| 决策 | 推荐方案 | 主要代价 |
| --- | --- | --- |
| 团队是否独占物理池 | 默认不独占；合规、强隔离需求才使用专属池 | 共享必须配套配额和请求公平性 |
| 谁分配资源 | 平台管理员直接分配，或审批团队/项目申请 | 租户 owner 不自动获得集群管理权限 |
| 如何兼顾保障与利用率 | 小规模不出借的保障容量，其余闲置额度允许借用 | 借出的额度无法承诺瞬间收回 |
| 底层组件 | 优先验证 Kueue 准入/借贷能力，kube-scheduler 放置 Pod；不自研通用调度器 | 在线 Deployment 排空与回收必须先做兼容性验证 |
| 自动回收是否杀在线请求 | 默认不强杀；先停止新派发，再排空 | 长请求可能延迟资源回收 |
| GPU 部署如何自动扩缩容 | 流量/队列做需求信号，TTFT/TPOT 做 SLO 反馈；结合预热、配额与排空 | 需模型/硬件压测定标，不能仅凭 CPU/GPU 利用率或延迟阈值保证 SLO |
| 首版 GPU 粒度 | 整卡；有条件时纳管预先配置好的 MIG profile | 不提供任意显存切分、透明 GPU 超卖 |
| 多集群是否合成一个池 | 每个 ComputePool 只属于一个集群；跨集群建立逻辑目录 | 跨集群切换是重新部署和切流，不是直接搬 Pod |

## 2. 当前基础与需要补齐的边界

以下来自当前工作区代码检查，不代表对正在运行的集群做了新的验收：

| 当前能力 | 依据 | 与本提案的关系 |
| --- | --- | --- |
| ModelDeployment 已有 replicas、resources、nodeSelector、tolerations | `rust/crates/kubernetes/src/api.rs` | 已能描述资源消费，但没有 computePoolRef、项目归属与资源授权 |
| Operator 已管理 Deployment、Service、可选 HPA/PDB/InferencePool | `docs/operator-lifecycle.md` | 复用现有 ownerReference、UID 校验及进程边界 |
| HPA 写 ModelDeployment 的 /scale，由 Operator 同步运行副本 | `docs/operator-lifecycle.md` | 保持单一副本写入链路；不能再增加控制器争抢 Deployment replicas |
| 项目归属 tenant；已有 tenant_memberships | `rust/crates/entities/src/project.rs`、`tenant_membership.rs` | 尚无本设计的 Team、项目成员和池授权体系 |
| 路由池来自 XSCOPE_ROUTE_POOLS_JSON | `rust/crates/control-plane/src/config.rs` | 这是推理入口注册表，不是 GPU/CPU 容量目录 |
| 集群代理入口首先检查 require_owner 后转发请求 | `rust/crates/control-plane/src/api.rs` 的 cluster_proxy | 尚不能作为完整的项目级部署授权；多团队上线前必须补对象归属检查 |

现有 InferencePool 并不是错误的对象，而是解决另一层问题。它描述提供推理服务的一组后端 Pod，通过选择器与 endpoint picker 关联，不负责给团队分 GPU。参见 [InferencePool 官方定义](https://gateway-api-inference-extension.sigs.k8s.io/api-types/inferencepool/)。

保留现有推理池；新增资源池层。不要把现有 InferencePool 改名后直接当作 ComputePool 使用。

## 3. 对象分层与请求链路

| 对象 | 解决的问题 | 典型内容 |
| --- | --- | --- |
| ComputePool，算力资源池 | 有哪些可调度资源 | cluster、节点集合、CPU/内存、GPU flavor、维护状态 |
| PoolGrant，资源授权 | 谁可以用、能用多少、是否可借用 | tenant/team/project、nominal、hardMax、protected、借贷策略 |
| ModelDeployment，模型部署 | 将资源变成运行实例 | 模型、版本、runtime、每副本资源、期望副本、资源池引用 |
| InferencePool，推理后端池 | 一次推理请求选择哪个兼容实例 | 同类模型/版本/运行配置的 Pod、EPP |
| ServingAccessGrant，调用授权 | 哪个项目可以调用已加载的模型 | 服务/版本范围、调用身份、请求配额与优先级 |
| RoutePolicy，流量规则 | 请求进入哪个已发布的推理池 | 模型、请求头规则、stable/canary、权重 |

```text
资源控制链路：
管理员 ── 分配/审批 ── Tenant → Team → Project 的 PoolGrant
                                      │
                         ModelDeployment 引用 ComputePool
                                      │
                         准入 → Pod 调度 → 模型就绪

请求数据链路：
调用方 → Pingora → 已发布的 Inference Gateway → EPP → 模型 Pod
        │          根据 RoutePolicy 选服务入口       │
        鉴权/调用授权/限流/计费上下文               KV-cache-aware 选实例
```

一个 ComputePool 可以承载多个项目的多个 ModelDeployment。一个项目也可以同时获准使用 CPU 开发池、GPU 生产池等多个资源池。推理池仍按服务兼容性、版本和隔离需求组织，不按团队一律复制。

必须分别处理两种共享：

- **多个部署共享算力**：关注准入、GPU/CPU/内存占用、扩缩容和借用回收。
- **多个项目调用同一模型服务**：关注请求公平性、并发、token 预算、上下文隔离和 SLO。

获得 ComputePool 使用权不自动获得其他项目模型的调用权；获得模型调用权也不代表可以在它的资源池创建部署。同一模型被 10 个项目调用，只计算一份实际部署的资源占用。

## 4. 组织、授权与申请流程

### 4.1 组织模型

建议 Tenant 表示组织级隔离边界，Team 表示组织内团队，Project 表示部署、调用和成本归属边界。

- 每个 Team 属于一个 Tenant；每个 Project 首版只属于一个 Team，且 tenant_id 必须一致。
- 用户通过成员关系获得权限；跨团队协作用项目成员授权表达，不改变项目唯一归属。
- 老项目迁移至所属 Tenant 的默认 Team；不修改 Keycloak 用户、密码和身份。
- 每个 `(cluster, project)` 建立平台管理的 namespace 映射；业务调用方不能自行指定任意 namespace。
- 租户之间可以经平台管理员批准共享物理池，但不共享数据库授权、Secret 或默认网络访问权。

PoolGrant 按 `ComputePool → Tenant → Team → Project` 形成预算树。父级额度是子级的包络，不是额外可消费的一份资源。团队获得 4 张卡、项目获得其中 2 张卡，不代表总共增加了 6 张卡。

项目可绑定多个池，每个池分别建立授权链。一个池也可授权给多个团队。管理员直接分配给项目时，必须同时验证已有父级预算；父级不足时，在同一个审批变更中明确调整父级，不能暗中绕过。

### 4.2 权限矩阵

| 操作 | 平台管理员 | Tenant owner / Team owner | 项目维护者 | 普通成员 |
| --- | --- | --- | --- | --- |
| 创建物理池、修改节点范围、安装组件 | 是 | 否 | 否 | 否 |
| 分配/批准资源授权、修改借贷策略 | 是 | 默认仅提交申请 | 否 | 否 |
| 为所属范围申请额度或变更 | 是 | 是 | 是，仅所属项目 | 否 |
| 在批准范围创建/扩缩模型 | 是 | 按项目管理权限 | 是 | 默认只读/调用 |
| 查看资源使用 | 全局 | 所属组织/团队汇总 | 所属项目 | 按显式授权 |
| 调用模型 | 仍需调用授权与 API key | 同左 | 同左 | 同左 |

首版不引入复杂的多级审批系统。管理员分配与管理员审批最终都生成同一种 PoolGrant；以后若授权团队 owner 分配子额度，必须限定在父包络内并增加审计。

### 4.3 两种入口，共用一种结果

**管理员直接分配**：选团队/项目 → 选资源池 → 设置资源向量和策略 → 校验所有祖先预算及池预算 → 生成版本化 Grant → 集群策略生效后标记 Active。记录 `source=admin_direct`，不伪造申请流程。

**团队/项目申请**：申请现有池使用权或扩额，填写 GPU 规格、CPU/内存、基础需求、峰值需求、在线等级、期限和理由。状态为 `Pending → Approved / Rejected / Cancelled`。批准会产生 Grant；审批通过与集群策略 Active 分开显示。请求重复提交以幂等键去重。

用户申请的是资源使用权。新增物理池、纳管新 GPU 节点是单独的管理员操作。

Grant 到期/撤销时先进入 `Draining`：禁止新建和新增占用，按批准的策略排空已有资源；只有资源释放被确认后才变成 `Revoked`。不能到期后删除记录，把仍运行的 Pod 当成“免费资源”。

## 5. ComputePool：容量与隔离模型

### 5.1 池的定义

每个池绑定一个 cluster_id、一组受控节点选择条件，以及一个或多个资源 flavor。Flavor 描述 GPU 型号、整卡/MIG 模式、profile、必要拓扑；CPU-only 也可以成为独立 flavor。

首版约束：

1. 同一节点只能属于一个可分配 ComputePool。上层可建立只读聚合目录，但不能重复发放底层容量。
2. 节点标签与选择条件由管理员维护，变更需检查集合交叉和现有分配；有运行负载时先维护/排空，不直接搬归属。
3. GPU 不同型号、MIG profile 分开核算，不能用“8 GPU”掩盖型号不兼容。
4. GPU 显存是设备规格与运行约束，不与主机 memory 混算；整卡模式不接受任意 `gpuMemory: 10Gi` 当成可调度资源。
5. 首版支持单 Pod 单卡/同节点多卡；跨节点分布式副本必须先验证 gang/topology 集成，不能把集群 GPU 总数相加就判定能运行。
6. 业务选择 ComputePool 和经过验证的运行规格，不能用自定义 nodeSelector、toleration、schedulerName、nodeName 越过池边界。

不同 GPU flavor 如果位于同一物理节点，CPU/内存仍只记一次。首版拒绝会将同节点通用资源重复复制到多个 flavor 预算的配置；混合 MIG profile 共宿时的共享主机预算，需要单独通过适配器验收。

### 5.2 区分“额度、占用、实际利用率”

容量以 Kubernetes allocatable 和有效 Pod requests 为基础，而不是节点规格标称值或 GPU 利用率曲线。

```text
可规划容量 = 池内有效节点 allocatable
           - 非本平台资源占用
           - 平台基础设施占用与尚未体现于 allocatable 的预留

可新增准入预算 = 可规划容量
               - 后端已锁定/已准入资源（含待启动/终止中）
               - 尚未被实际对象覆盖的有效容量预留
```

实现须按 UID 对齐“预留 → 实际 Pod”计数，不能同一份资源扣两次；已被 kubelet 从 allocatable 扣掉的系统预留也不能重复扣减。未知归属 Pod 按外部占用处理，不为让数字好看而忽略。

有效请求量包含整个 Pod 的容器、init/sidecar 语义、Pod overhead 和实际注入资源。发布 surge、旧版本终止中实例、新模型加载中的实例都消耗预算。模型 EPP、Envoy、监控等 CPU/内存列为服务开销或平台开销，不能漏算。

实际调度仍由 kube-scheduler 判定；总量足够不代表单节点资源、GPU 拓扑、亲和性一定满足。准入成功后仍可能 `PlacementPending`，不能显示为资源已经可用。

监控同时展示：

- 授权额度、已准入量、运行量、Ready 量、借用量、排空中量。
- requests/allocatable 占用率与真实 CPU/GPU 活跃度，二者分开。
- 显存占用、KV cache 使用、有效吞吐和延迟达标率。

不要因为某个 GPU 当前利用率只有 10%，就把已经分配的整卡再次当成 0.9 张空闲卡出售。

### 5.3 安全边界

ResourceQuota/LimitRange 用于 namespace 上限和默认资源约束，但不承担跨团队借贷、节点隔离或物理容量保证。Kubernetes 的 namespace quota 独立于集群实际容量，不能将它视为资源预留。参见 [Resource Quotas](https://kubernetes.io/docs/concepts/policy/resource-quotas/)。

项目 namespace、RBAC、NetworkPolicy、受控 Pod 模板和准入校验共同构成边界。标签只是归属信息，不是授权凭证。GPU 共节点不等于安全沙箱；对不可信自定义镜像或强隔离租户，应选择专属节点/独立集群等经安全评审的运行方式。

## 6. 配额、保障与借用语义

所有资源额度都是向量，例如 `{cpu: 32, memory: 128Gi, a100-80gb/full: 4}`；每一维都要满足约束。GPU 用整数或受支持的设备实例数，CPU 使用整数 millicores，内存使用整数 bytes，数据库不以浮点数记账。

| 字段 | 含义 |
| --- | --- |
| `nominal` | 基础权益；竞争时具有恢复至本额度的优先权，不代表当前空闲硬件已为你保留 |
| `hardMax` | 含借用、启动中、发布额外实例等的最高准入量；不是吞吐承诺 |
| `protected` | nominal 中不向外出借的额度；用于保障副本/预留启动容量 |
| `lendingLimit` | nominal 中最多允许出借的量，不超过 nominal - protected |
| `borrowingLimit` | 超过 nominal 的最大借入量，不超过 hardMax - nominal |
| `fairWeight` | 同等级争抢弹性资源的相对权重，不表示 GPU 利用时间的精确百分比 |
| `expiresAt` | 授权期限；到期按排空流程回收 |

每个授权满足 `0 ≤ protected ≤ nominal ≤ hardMax`。项目实际准入量同时计入其 Team、Tenant 和池，每个祖先的 hardMax 都要成立。

同一父级下子级 nominal 之和不能超过父级 nominal；未分配差额可以作为父级公共弹性额度。子级 protected 总和也必须受父级 protected 约束，不能在父级允许出借的额度上再承诺子级独占保障。子级 hardMax 之和可以大于父级 hardMax，但必须由准入后端在运行时强制执行父级总上限，不能只做创建时静态检查。若选定后端无法正确表达，首版必须拒绝该配置或限制子级 hardMax 总和，而不是静默弱化隔离。

池的 nominal 总发放不得超过经核定的规划容量。节点故障造成容量下降时保留权益记录，展示 `CapacityDegraded`，暂停无容量支撑的新准入，不能通过删配额掩盖违约。

### 6.1 三种服务等级

| 等级 | 资源行为 | 适用场景 |
| --- | --- | --- |
| Protected | 不参与常规借用回收；可保持最小 Ready 副本 | 关键在线服务 |
| Elastic | nominal 内优先，超出部分借用；借用实例必须支持排空回收 | 普通在线峰值、可弹性服务 |
| BestEffort | 可以没有 nominal，使用可借空闲量；无启动时间保证 | 开发、评测、可重试工作 |

Protected 不是永不故障：设备、节点、网络故障仍需冗余。只保留 quota 也不保证模型瞬间加载；严格启动/延迟要求必须保留足够的暖副本和合适的节点拓扑。

借用是有记录的临时准入，不是无上限超卖。已有被借用的实例不能在不重新审批预算的情况下变成不可回收保障副本。

### 6.2 8 张 GPU 示例

假设同一池 8 张相同 GPU，每副本 1 GPU，CPU/内存等其他维度均足够；A、B 各有 nominal=4、hardMax=8、protected=2，允许出借各自剩余 2 张额度。

| 时刻 | A | B | 行为 |
| --- | --- | --- | --- |
| 初始 | 使用 4 | 使用 2 | B 尚未使用的 2 张额度可出借 |
| A 峰值 | 使用 6，其中借用 2 | 使用 2 | 总计 8，不为 B 空置那 2 张可出借容量 |
| B 要恢复 4 | 目标回到 4 | 期望 4，实际仍为 2 | A 借用实例开始排空，B 暂时等待 |
| 回收确认 | 使用 4 | 使用 4 | A 释放设备后，B 的新实例才能准入并加载模型 |

如果 A 的请求暂时无法排空，B 的扩容就会延迟。想让 B 随时立即使用 4 张卡，需要把 B 的 protected 提升到 4，并接受这些容量可能闲置；还要解决模型冷启动时间。**瞬时硬保障、全部空闲容量出借、不打断任何长请求，三者不能同时无条件满足。**

## 7. 第三方复用与准入后端选择

### 7.1 推荐分工

| 组件 | 责任 | 不承担 |
| --- | --- | --- |
| XScope 控制面（Rust/SeaORM） | 组织、申请审批、授权版本、业务意图、审计 | 节点放置算法、GPU 驱动 |
| cluster-agent（Rust/kube-rs） | 本集群资源观测、授权策略投影、状态回传 | 全局身份管理、账务扣款 |
| Operator（Rust/kube-rs） | 模型资源收敛、副本组、发布/排空、归属校验 | 自行决定跨租户抢占公平性 |
| Kueue（优先候选） | 工作负载准入、资源 flavor、共享配额和借贷仲裁 | 业务审批、在线 SSE 安全排空 |
| kube-scheduler | 将已允许运行的 Pod 放置到合适节点 | 平台成员权限、计费 |
| GPU Operator / device plugin | 驱动、设备发现、受支持的 GPU 资源暴露 | 项目业务授权 |
| llm-d EPP | 请求级实例选择及经验证启用的流控 | 团队 GPU 配额分配 |

Kueue 已提供 nominalQuota、borrowingLimit、lendingLimit 与共享队列语义，适合作为复用对象。参见 [ClusterQueue](https://kueue.sigs.k8s.io/docs/concepts/cluster_queue/)。

**选择策略：先验证 Kueue，而不是先实现一套自己的公平调度器。** 每个池只允许一种准入后端；Kueue 生效时，由 Kueue 的 admitted Workload 状态作为实际资源准入依据，PostgreSQL 中的 allocation 是业务关联与观测镜像，不再独立发放同一批 GPU。

准入前后还要区分 `QuotaReserved` 与 `Admitted`：后端已经锁定 quota、但仍等待 admission check 的 Workload 也占预算，不能只统计 Running/Admitted。受保护但未运行的预留由队列不出借策略或显式预留表达，不能因为没有 Pod 就重新算成共享空闲量；镜像中对这些状态分别计数且不重复扣减。

### 7.2 组织映射草案与必须验证的细节

- ComputePool/flavor 映射到受控 ResourceFlavor 和同一共享资源域；project namespace 通过 LocalQueue 提交。
- 项目额度映射到叶子 ClusterQueue；Tenant/Team 的聚合借贷边界优先映射到 hierarchical Cohort。
- protected 与可借弹性额度使用不同策略域/队列，避免借用回收影响保障实例；两者合计不得重复发放 nominal。
- 父级包络不是额外库存。Kueue 的 Cohort nominal 是子队列之外的增量资源，不能把 XScope 父级 nominal 原样填入 Cohort 再在子级填一遍。仅未向子级分配的差额可以作为父级增量。参见 [Cohort](https://kueue.sigs.k8s.io/docs/concepts/cohort/)。

上线前必须固定组件版本，并用生成清单验证“父级硬上限、子级峰值超配、protected 不出借、CPU/GPU 联合约束”四类反例。不能假定 Cohort 名称树天然等同于本设计的完整授权树。

### 7.3 Deployment 兼容性是评审后的第一道技术门槛

Kueue 当前文档中的 Deployment 管理基于逐 Pod 准入，并非把整个 Deployment 当成一个原子 Workload；因此可能只有部分副本获准运行。参见 [Run Deployment](https://kueue.sigs.k8s.io/docs/tasks/run/deployment/)。

必须验证：

1. HPA 扩容、排队 Pod、缩容与副本重建不会绕过授权或发生不断重建/抢占循环。
2. protected 和 elastic 副本能分别入队，仍属于正确模型和推理池。
3. 借用回收时能先执行在线排空，再回收设备；上游抢占不得提前删除尚未排空的在线实例。
4. 发布 surge、Pod 删除中的资源、重试 admission 不重复占用或提前释放。
5. 每项目多个模型竞争时不会被一个无限扩容的 Deployment 长期垄断；版本支持的公平策略须实测。
6. 配置变更、组件重启和集群断连后，观测镜像能从权威 Kubernetes 状态重建。

若第 3 条不成立，**在线池保持 borrowing/preemption 关闭**，仅交付固定额度、共享模型调用、明确的扩缩容上限；允许中断的工作负载可单独验证借用。不得为了赶进度启用“先删 Pod 后解释”的在线回收，也不默认转向自研完整调度器。需要改变准入后端时另写 ADR 评审。

## 8. 模型部署、扩缩容和回收协议

### 8.1 合同扩展

拟在业务部署请求增加 `projectId`、`computePoolId`、`resourceFlavor`、`capacityClass`、`protectedReplicas`；由服务端解析对应的有效 Grant、namespace、允许的资源规格，并投影到 ModelDeployment。用户不能填写任意 grant revision 或准入结果。

`protectedReplicas` 是被批准的常驻基线，不等于 HPA 最大副本。基线资源总和不得超过项目 protected；启用 HPA 时其 minReplicas 不得小于此基线。借用部署无法靠增大 HPA minReplicas 获得新的保障权利。

状态必须区分：

| 字段/状态 | 语义 |
| --- | --- |
| desiredReplicas | 用户或 HPA 的期望，保留现有 spec.replicas 语义 |
| admittedReplicas | 当前准入后端批准可运行的副本量 |
| replicas | 实际观测到的副本量，保留现有 status.replicas 语义 |
| readyReplicas | Runtime 已就绪量，不等同于公共入口健康 |
| ServingReady | 推理入口/EPP/路由注册均完成后的独立条件 |
| QuotaPending / PlacementPending / Warming | 等配额 / 等放置 / 等模型加载 |
| Borrowed / Reclaiming / ReclaimBlocked | 借用中 / 排空回收中 / 回收受阻 |

控制面不偷偷把用户期望值改成当前可用量。HPA 继续写 ModelDeployment `/scale`，Operator 负责子资源；Kueue 模式可以存在未获准运行的排队 Pod，不能把“已创建”当作“已准入”。其他模式也必须遵守同样状态合同。

### 8.2 副本隔离与安全缩容

目标是同一模型版本可有 `protected` 与 `elastic` 两组子 Deployment，由同一个 ModelDeployment 所有；兼容的 Pod 可以由同一个 InferencePool 选择。Operator 分解总期望副本，HPA 不直接管理两组子 Deployment。

这种拆分不是当前已实现行为。已有单 Deployment 不原地强制改不可变 selector；采用显式迁移计划，保持旧资源归属和业务可用性。

借用回收流程：

```text
提出回收意图 → 冻结受影响弹性组新增准入
            → EPP/Runtime 停止向待回收实例派发新请求并确认
            → 等在途 SSE/推理完成或按业务策略取消
            → 缩小子 Deployment、确认 Pod 与设备占用释放
            → 准入后端确认额度归还 → 等待者可获准运行
```

首版若不能精确控制 Deployment 缩容删除哪个 Pod，应保守排空整个受影响的弹性组，再缩组并恢复保留副本派发。不能仅依赖 Pod deletion cost 之类的建议性排序承诺“不删错正在执行的实例”。该方案会暂时降低弹性组吞吐，需在验收中量化。

默认超时只标记 `ReclaimBlocked` 并告警，不强杀关键在线请求；best-effort 可在管理员批准的强制截止时间后终止，并提供清晰的失败/重试语义。排队、取消、断连与计费事件必须使用一致 request_id。禁止对已输出内容的 SSE 透明整段重放。

PDB 不能代替上述协议：它不约束所有缩容/删除路径。参见 [Kubernetes PDB](https://kubernetes.io/docs/tasks/run-application/configure-pdb/)。

### 8.3 发布与弹性控制

- stable/canary 的 GPU 占用分别记账；10% 流量不等于只需要 10% GPU。
- 发布前检查新旧版本共存、maxSurge 与终止中实例的容量预算；没有余量则等待或由用户明确接受降容量发布，不越过 hardMax。
- 整池满载时，不保证零中断更新仍可无额外容量完成。生产池保留发布/故障余量，额度需显式登记。
- GPU 服务优先评估排队请求/token、TTFT、活跃序列、KV cache 压力与 SLO；CPU HPA 保留为现有基础能力，不把 CPU 指标等同 GPU 容量。
- 本仓库 pending-request 自定义指标链路仍需联调；不得把配置字段存在当作可用能力。
- 缩容需要稳定窗口与模型冷启动成本评估；scale-to-zero 仅作为低优先级、接受冷启动服务的后续选项，不宣称现有 HPA 已支持。

### 8.4 标准 LLM 部署必须包含 SLO 自动扩缩容合同

上一版仅列出候选指标，不足以构成 GPU 弹性策略。现在明确补充以下设计要求，细节与配置草案见 [LLM SLO 自动扩缩容](llm-autoscaling-design.md)：

- 需求前馈：可信到达流量、输入工作量、活跃请求与分层队列，不能只看已完成请求的 QPS。
- SLO 反馈：分别定义入口 TTFT、运行时 TTFT、请求级 TPOT 与 token 级 ITL；延迟超标须结合拥塞证据，避免扩容无法解决的瓶颈。
- 快扩慢缩：预留暖副本，计算已在途扩容，使用滞回、稳定窗口和安全排空；无指标不等于无流量。
- 控制权：首选验证 llm-d EPP + Prometheus + KEDA，由唯一 HPA 写 ModelDeployment `/scale`；启用 KEDA 时 Operator 不再创建竞争的原生 HPA。Kueue 仍只管理资源准入。
- 有界保障：扩容请求不突破团队/项目授权，资源不足时明确暴露 `CapacityBlocked` 和 SLO 风险；不能用持续增加 Pending Pod 伪装扩容成功。
- 部署边界：按模型版本、池及运行规格定标；stable/canary 分别评估，后续 P/D 分离分别扩 prefill/decode，不平均稀释瓶颈。

GPU 在线发布验收必须包含真实负载下的自动扩容、缩容与 SLO 测试。按既定后端阶段落地依赖，并不意味着“标准 LLM 部署”可以在缺少此闭环时宣称已完整实现。

## 9. 提高利用率的优先顺序

1. **先减少重复加载。** 模型、版本、运行配置及安全域兼容时，提供共享模型服务，让多个项目通过调用授权使用；不因项目不同就复制整套模型。私有权重、适配器或数据隔离要求不满足时不能强行合并。
2. **基础保障小而真实，弹性上限可借用。** 根据业务 SLO 给暖副本，不按峰值永久占卡。通过离峰评测等可回收负载消耗空闲额度。
3. **改善运行效率。** 依赖运行时 continuous batching、llm-d 路由等能力；实际性能通过压测确定，不在 Pingora 重写推理调度。
4. **减少资源碎片。** 标准化 GPU flavor 和 CPU/内存配比，基于现有调度器策略评估装箱；不能为了填满单机牺牲关键服务的故障域分散。
5. **缩短弹性生效时间。** 模型工件缓存、预拉镜像和必要暖副本优先；配额闲置但模型加载过慢，同样无法满足突发需求。
6. **最后再评估 GPU 细分。** 支持预配置 MIG；time-slicing 不作为首版默认生产隔离机制。

NVIDIA time-slicing 不提供 MIG 那样的显存与故障隔离，多个共享资源请求也不等于成比例算力保证。因此不能把 time-slicing 的逻辑副本数显示为真实 GPU 卡数。参见 [NVIDIA GPU Sharing](https://docs.nvidia.com/datacenter/cloud-native/gpu-operator/latest/gpu-sharing.html)。

目标不是“所有卡持续 100%”，而是**在满足已承诺 SLO 的前提下，提高有效吞吐/卡时，减少不必要的空置与重复副本**。评价需同时看 p95/p99 TTFT、每 token 延迟、成功率、超时率和回收时间。

### 9.1 共享模型的请求公平性

Pingora 根据可信 API key 推导 tenant/project/服务等级，移除外部伪造的公平标识和优先级头，再注入内部身份。入口限流解决“允许进多少”，EPP 负责“进入后哪个请求先派发、去哪个实例”。

llm-d Flow Control 提供按优先级及 flow 排队的能力，但需要显式启用并验证；严格高优先级可能让低优先级长期等待，因此高等级入口本身也必须限流。队列为内存状态，不能承诺重启后不丢排队请求。多 EPP 副本的全局公平与精确 GPU 时间份额也不能从单实例队列推导。参见 [llm-d Flow Control](https://llm-d.ai/docs/architecture/core/router/epp/flow-control)。

KV-cache 复用策略还需定义信任域：不因资源池共享就默认允许跨安全域共享可识别的缓存上下文。应验证运行时支持的隔离/缓存命名能力和潜在侧信道风险。

## 10. 持久化、API 与一致性

### 10.1 数据模型草案

业务数据继续使用 PostgreSQL + SeaORM；Rust 类型放在现有 workspace 下。新增 Kubernetes 类型继续使用 kube-rs。资源分配记录与货币双分录账本分开。

| 表/对象 | 核心字段与约束 |
| --- | --- |
| teams / team_memberships / project_memberships | tenant_id、角色、项目唯一团队归属；跨组织引用拒绝 |
| compute_pools | id、cluster_id、selector、flavors、维护策略、revision；集群内节点归属不得交叉 |
| pool_grants | pool_id、subject_type/id、parent_id、资源向量、策略、期限、revision、desired/effective 状态 |
| pool_access_requests | 申请人、目标项目/团队、期望额度、理由、审批结果、关联 grant revision |
| project_cluster_bindings | project_id、cluster_id、namespace UID；唯一映射，禁止仅按 namespace 名认领 |
| capacity_allocations | deployment UID、Pod/Workload UID、grant revision、资源规格 hash、后端、阶段、观测版本 |
| pool_policy_outbox | policy generation、目标集群、幂等键、投影状态；与 Grant 变更同事务提交 |
| pool_audit_events | 主体、对象、前后值、理由、关联请求/策略版本；敏感字段脱敏 |
| serving_access_grants | 调用项目、服务/版本、安全域、等级；与 ComputePool 授权独立 |

必要约束：父级必须属于同池且组织路径合法；授权不能形成环；同主体同池只有一个当前生效版本；allocation 按后端对象 UID 幂等；资源数量非负并防溢出。Grant 变更在 PostgreSQL 事务中锁定池/相关祖先并使用 revision CAS，避免两个管理员同时过量分配权益。

### 10.2 API 草案

| API | 行为 |
| --- | --- |
| `GET /api/v1/compute-pools` | 返回用户可见池及自身授权摘要，不泄露其他团队模型/用量明细 |
| `POST /api/admin/v1/compute-pools` | 管理员创建资源池配置，不自动纳管未知节点 |
| `PUT /api/admin/v1/compute-pools/{id}` | expectedRevision 校验；有运行资源时拒绝危险归属修改 |
| `POST /api/v1/pool-access-requests` | 提交使用/扩额申请，需所属项目管理权 |
| `POST /api/admin/v1/pool-access-requests/{id}/decision` | 管理员批准或拒绝，返回 Grant/审批状态 |
| `POST /api/admin/v1/pool-grants` | 管理员直接分配；校验父包络和唯一性 |
| `PUT /api/admin/v1/pool-grants/{id}` | 版本化调整；缩额可能进入排空而非立即 Active |
| `GET /api/v1/projects/{id}/capacity` | 分池返回期望、授权、准入、Ready、借用与等待原因 |

写请求使用幂等键，版本冲突返回 409；无权访问返回 403 或按防枚举策略返回 404。异步生效返回 202 和 operation_id，不能返回“已分配可用 GPU”。池停用、缩额、撤销提供 dry-run，列出受影响模型与可回收量。

以下是业务 Grant 草案，不是 Kueue CRD 或现有可执行 API 请求：

```yaml
poolId: gpu-prod-a
subject: {type: project, id: project-search}
parentGrantId: team-search-gpu-prod-a
expectedParentRevision: 7
resources:
  a100-80gb-full: {nominal: 4, hardMax: 8, protected: 2, lendingLimit: 2, borrowingLimit: 4}
  cpu: {nominal: "32", hardMax: "64", protected: "16", lendingLimit: "16", borrowingLimit: "32"}
  memory: {nominal: 128Gi, hardMax: 256Gi, protected: 64Gi, lendingLimit: 64Gi, borrowingLimit: 128Gi}
policy:
  fairWeight: 1
  borrowedWorkloadClass: drainable
  forceTerminateOnDrainTimeout: false
source: admin_direct
```

### 10.3 策略与占用的不同真相源

**PostgreSQL 管授权意图，集群准入后端管实际 admission，Kubernetes 观测管实际运行，三者分别展示版本与状态。**

```text
Grant 事务提交 + outbox
  → cluster-agent 校验新 generation、投影队列/策略
  → ACK effectiveGeneration
  → 允许使用新策略提交/扩容工作负载
  → 观测后端 admission 与 Pod UID，更新 allocation 镜像
```

缩额需要先禁止增加相关占用，再排空超额资源，最后确认目标版本生效。不能用一个“最后写入成功”的时间戳掩盖集群仍执行旧策略。

涉及多个队列/父级的策略修改不是 Kubernetes 原子事务：先在集群侧冻结受影响范围的新准入，再应用整组配置，验证一致后 ACK 并解冻。延迟提交的旧模板也必须经过集群侧当前授权/版本校验；只在控制面校验一次，不能防止撤销后旧任务继续创建。

不得依据 TTL、控制面重启、Pod 长时间 Pending 或 ACK 丢失就把可能仍占用的资源再次分给别人。只有明确未下发的意图可以直接取消；已经下发的操作，必须确认撤销、对象终止及后端额度释放。断连时保持 Unknown 占用，限制新增，不凭陈旧镜像发容量。

晚到 ACK 按 generation/对象 UID 丢弃；删除旧部署后同名新建不能继承旧 allocation。策略投影只更新自身拥有的对象，保留现有 UID/resourceVersion 前置检查，不接管外部同名队列或资源。

## 11. 多集群、成本与低开销运行

### 11.1 多集群

- ComputePool 永远标明 cluster_id。`gpu-production` 可以是跨集群目录分组，但实际 Grant 和 allocation 必须落到具体池。
- Tenant/Team 可有跨集群总预算，但首次落地采用明确的各集群子额度，总和不超父包络；不在集群间同步借用同一份容量。
- 未来跨集群改配：先冻结/回收源集群额度并确认，再向目标集群增额；要求 desired state、heartbeat、generation、ACK 以及防重复发放协议就绪。
- 源集群断连时不得假定 Pod 已停止，也不得回收其账面额度。灾备容量必须预先规划并独立计入预算。
- 跨集群部署新版本并预热、验证 ServingReady 后再由 RoutePolicy 切流；模型工件、Secret、网络、数据合规分别检查。

### 11.2 成本归属

硬件容量授权不是钱包余额，也不是 API token 限额。资源占用可以按 flavor 的 allocation-seconds 形成成本视图，推理调用按现有用量事件形成调用费用视图，避免把两者混成一张“配额表”。

共享模型的硬件成本首先归 hosting project/平台服务账号，再按明确的调用分摊政策归集；不能把每个调用项目都记为占有整份 GPU。是否对闲置 protected 容量收费、内部成本展示是否产生应付金额，需要单独的计费评审。支付/税务不在本设计实现范围。

### 11.3 保持低 CPU 开销

不为每个团队/项目启动常驻 allocator/proxy。复用现有控制面、cluster-agent、Operator 的进程边界，用 watch/cache、变更驱动 reconciliation、合并更新与退避降低开销。队列/Grant 是数据对象，不是一份新微服务。

Kueue 等组件只在需要的集群部署共享实例，并实测 requests/limits；不能在不了解集群规模时承诺固定毫核数。EPP/Inference Gateway 按服务拓扑配置，不因“获得资源池授权”就自动启动一套。当前安装层持有的 EPP/Envoy 资源继续由安装层管理，不偷偷改 owner。

## 12. 分阶段实施与迁移

本次到文档交付为止。评审通过后按仓库既定后端顺序推进；不是并行扩展所有 UI 功能。

| 阶段 | 交付内容 | 退出条件 |
| --- | --- | --- |
| A：设计确认与验证计划 | 确认名词、组织模型、保障等级；固定 Kueue 验证版本与范围 | 本文关键决策获确认；在线排空试验方案明确 |
| B：补齐阶段 4 资源所有权与授权 | 项目绑定、受控 namespace、ComputePool 目录、管理员 Grant；单集群固定配额；Kueue 适配验证；SLOPolicy/弹性模式与唯一 HPA 所有权合同 | 越权测试、重复节点归属测试、HPA 配额边界通过；GPU SLO 指标适配与定标计划就绪 |
| C：结合阶段 5 一致性基础 | allocation 观测、事务 outbox、资源回收状态；与 SSE/取消、用量事件关联 | 重启/重复事件/丢 ACK 不双分配、不重复记用量 |
| D：阶段 6 多集群基础 | 各集群独立额度、desired/effective generation、heartbeat/ACK | 断连冻结、晚到 ACK、幂等重放通过 |
| E：阶段 7 SLO 驱动共享 | 流量/队列 + TTFT/TPOT 弹性闭环、预热与需求预测、请求公平、排空回收、借用灰度、指标与审计 | 真实 GPU 的扩缩容/SLO/隔离/回收压测通过，再启用相应在线能力 |
| F：之后的业务扩展 | 资源成本分摊与阶段 8 计费/支付接口衔接 | 单独评审，不借此提前扩展支付或 UI 合同 |

阶段 B 可以验证借用机制，但在线自动回收不早于取消链路、持久化恢复和所需 SLO 观测通过。若 Kueue 的兼容性门槛未通过，保持已验证的固定额度子集，不宣传完整弹性共享已实现。

迁移规则：

1. 先将现有模型、InferencePool、路由注册表与 namespace 做只读盘点，显示为 `legacy/unassigned`。
2. 管理员确认现有资源归属后，建立默认 Team、project/cluster 绑定和覆盖现有占用的 Grant；禁止扫描后自动认领外部资源。
3. 新创建模型必须带合法项目/资源池引用；旧模型在切换前保持原逻辑，不因新 quota 默认值为 0 而被删除。
4. 显式迁移并逐项核对现有 UID、资源 requests、EPP 绑定和公开路由；新旧准入后端不能同时给同一对象发容量。
5. 回滚先冻结新增、确认占用再恢复旧模式；不能删除准入门禁而放开无限创建。保留授权、审计与原配置备份。

UI 等后端合同验证后再做：资源池目录、团队/项目额度、申请/分配、等待原因、借用/回收进度。现有路由页保留“推理后端池”名称，新增资源管理页使用“算力资源池”，不将两种池合并成一个表单。

## 13. 验收清单

所有未来构建、测试、生成与打包通过 Bazel targets；不建立 cargo/npm 等旁路。下面是待实现验收要求，不是已执行结果。

| 场景 | 必须观察到的行为 |
| --- | --- |
| 两管理员同时分配最后一份 nominal | 一个成功，另一个冲突/预算不足；父包络不超发 |
| 两项目争抢最后一张可准入 GPU | 只一个实际获准；另一个有可解释的等待状态 |
| Team=4，两个项目各 hardMax=4 同时申请 | 团队合计实际占用不超过 4；不按叶子单独放行 |
| 修改 cluster/namespace/project 或伪造 queue label | 请求拒绝，不能跨租户读写或绕过门禁 |
| 节点同时匹配两个池 | 拒绝重叠配置/冻结受影响新增，不重复计数 |
| CPU 或内存不足但 GPU 有空闲 | 等待正确资源维度；不得只检查 GPU |
| GPU 总数足够但单机/拓扑不满足 | PlacementPending，而非 ServingReady |
| HPA 连续扩容、指标抖动 | 不超 hardMax、不争抢子 Deployment；状态解释准入限制 |
| QPS 不变但长上下文/长输出增加 | 按实际工作量、队列与 SLO 识别压力，不误判为需求不变 |
| TTFT/TPOT 超标、新副本仍在预热 | 区分瓶颈、在途容量和容量受限；不持续放大同一扩容缺口 |
| 指标中断、低样本或仅出现慢客户端 | 不把缺数当空闲，不盲目扩缩 GPU；详细验收见弹性设计 |
| 稳定/灰度/surge/终止中 Pod 同时存在 | 全部按有效占用计数，不提前释放旧版容量 |
| A 借用、B 恢复 nominal | A 保障组不动；弹性组确认排空后归还，B 才新增准入 |
| 长 SSE、客户端取消、排空超时 | 请求状态可追踪；默认不强杀，blocked 有告警；用量不重复 |
| 先有 policy intent，随后断连/重启/重复 ACK | 版本单调，陈旧 ACK 无效；未知占用不当成空闲 |
| 删除旧对象后同名重建 | UID 隔离，旧授权状态不附着新实例 |
| EPP 重启、多个 EPP、伪造优先级头 | 明确队列失败语义；不虚报全局公平，外部身份头不可信 |
| Grant 缩额/撤销、节点故障/外部占用增加 | 冻结新增、保留当前占用证据、展示退化或排空，不静默删业务 |
| 原有模型/Keycloak/其他应用 | 不重置身份数据、不接管资源、不改变无关应用 |

利用率验收采用相同硬件、模型、流量和 SLO 的对比实验：固定专属、共享固定额度、共享借用三组；记录有效 tokens/s/GPU、卡时、暖机时间、队列等待、p95/p99 延迟和失败率。具体达标阈值在业务 SLO 确认后制定，不能预先承诺某个 GPU 利用率百分比。

本地 CPU/Echo 能验证对象生命周期和配额逻辑，不能证明 GPU 隔离、KV-cache-aware 收益或真实回收性能；这些需真实 GPU 环境验收。

## 14. 评审后需要确认的业务参数

- “租户”是否就是公司/组织，团队是否只在租户内部？本文默认是。
- 哪些服务必须有暖副本和不出借保障，哪些可接受排队/冷启动？本文默认小保障、大弹性。
- 哪些团队可以共享已加载模型，哪些要求独立权重、缓存、节点或集群？默认显式授权，不自动合并。
- 管理员直接分配是否为主要流程？本文默认支持，申请审批为同级入口。
- 在线回收默认不强杀是否合适？本文默认不强杀；强制截止仅允许显式批准的可中断任务。
- 真实 GPU 型号、数量、集群范围以及业务 SLO 是什么？这些决定 flavor、保障额度和压测阈值，不影响前述对象分层。

建议先确认第 1 节决策，再推进第 7 节 Kueue 兼容性验证和第 12 节阶段 B；本 RFC 不把尚未验证的在线借贷当作确定可上线能力。
