# RFC 补充：面向流量与 TTFT/TPOT 的 LLM 自动扩缩容

状态：方案方向已确认，分阶段实施；日期：2026-09-05。属于 [算力资源池设计](compute-pools-design.md) 的必需补充。用户已明确选择 **KEDA 作为唯一自动扩缩容配置入口**，不再由平台直接生成独立 HPA。KEDA 内部仍通过其管理的 HPA 执行水平扩缩容。首批实现范围和验收记录见 [KEDA 接入](keda-autoscaling.md)；本文未勾销的能力仍是设计目标。

## 1. 结论：从“有 HPA”升级为“LLM 服务弹性闭环”

标准 GPU LLM 部署需要：**流量/工作量前馈 + 排队/并发压力 + TTFT/TPOT 反馈 + 冷启动补偿 + 配额准入 + 安全缩容**。CPU HPA 不足以表示这一能力；仅添加一个延迟阈值也不够。

设计起点是 CPU HPA。首批实现改为 Operator 管理 KEDA ScaledObject，新增 EPP running 指标配置，保留 CPU 基线验证路径；在在线排空接入前禁用自动缩容。EPP 指标链路仍待联调，没有本文的 SLOPolicy、延迟反馈、预热感知、分阶段瓶颈判断和预测能力。未据此宣称当前集群已具备 GPU 自动弹性。

本文将 SLO 合同纳入模型部署基础设计，按仓库既定阶段逐步实现其依赖。最终上线标准在线 GPU 服务前，必须验收闭环；不能以“后续优化”为由长期只保留 CPU 指标。

## 2. 三个控制层，职责不能混淆

| 层级 | 决策 | 典型时标 | 责任组件 |
| --- | --- | --- | --- |
| 请求层 | 是否排队、限流、降载，选哪个实例 | 每请求/毫秒到秒 | Pingora、Inference Gateway、llm-d EPP、runtime |
| 服务层 | 需要多少模型副本，是否提前预热 | 秒到分钟 | 指标/策略 + KEDA（内部管理 HPA）+ ModelDeployment Operator |
| 资源层 | 批准多少 GPU/CPU/内存、是否借用；是否需要更多节点 | 秒到分钟，节点供应可能更久 | PoolGrant、Kueue、集群基础设施 |

时标是设计上的职责划分，不是性能承诺。新增 GPU 节点与增加模型副本是两种操作；本地固定 GPU 无法凭扩容策略生成新卡。

```text
可信需求 / EPP 队列 / runtime 指标 / SLO 观测
                ↓
        指标校验、分桶、平滑与策略
                ↓
          KEDA → 唯一 HPA
                ↓
       ModelDeployment 期望副本
                ↓
       Operator + Kueue 准入
                ↓
      放置 → 加载/预热 → ServingReady
                ↓
           实测 SLO 反馈
```

扩容在未来增加容量，不会自动加速一个已经在旧 Pod 上执行的请求。突发时，现有副本的请求门禁和暖容量必须先保护服务。

## 3. SLO 与指标合同

### 3.1 时间口径先统一

| 指标 | 本平台建议定义 | 用途与限制 |
| --- | --- | --- |
| 入口 TTFT | 平台入口完整接收有效请求，到向下游发送首个有意义输出的时间 | 包括平台内部排队；不包括客户端到入口的全部网络耗时，不能称为客户端真实端到端 TTFT |
| Runtime TTFT | 模型服务接收请求到首 token 的时间 | 诊断 runtime 内排队/prefill；不能覆盖 EPP 等上游队列 |
| 请求级 TPOT | N>1 时，运行时首末输出 token 的时间差 / (N-1) | 每请求一次观测；N≤1 为不适用，不记 0 |
| ITL | 相邻实际生成 token 的间隔分布 | 观察生成过程卡顿；是 token 加权口径，不等于请求级 TPOT 分位数 |
| 队列等待/最老请求年龄 | 分别记录 EPP 与 runtime 等待 | 比完成态延迟更及时，也能发现尚无请求完成的拥塞 |
| Goodput | 在约定 TTFT/TPOT/成功率条件内完成的有效请求或 token 吞吐 | 与原始吞吐一起评价，避免通过大量拒绝“优化”延迟 |

首个 SSE 事件可能只是 role/metadata，不能当作首 token；网络 chunk 也可能含多个 token，不能用 chunk 间隔冒充 ITL。非流式 API、工具调用、多模态任务需独立定义 SLO；未获得 token 时间戳时只提供能够真实测量的口径。

运行时输出计数、推测解码和缓存统计依赖版本，必须固定镜像并做适配合同测试。vLLM 提供 TTFT、请求级输出 token 时间、ITL 和队列时间等指标，但各版本名称/语义可能变化，不能凭旧名称拼接规则。参见 [vLLM Production Metrics](https://docs.vllm.ai/en/latest/usage/metrics/)。

### 3.2 信号按角色使用，不全部当成扩容按钮

| 信号 | 角色 | 防误判要求 |
| --- | --- | --- |
| 合法到达率、请求长度分布、输入 token 工作量 | 前馈需求 | 区分鉴权/预算拒绝与容量拒绝；非法流量不能触发扩容 |
| 排队请求、待处理 token、最老队龄 | 积压和紧迫性 | EPP 外层队列与 runtime 队列分别看，不漏掉上游压力 |
| 活跃序列、并发、实测/估计剩余 decode 工作量 | 在途需求 | 完成请求的输出 tokens/s 是产出，不等于被压抑的真实需求 |
| p95/p99 TTFT、请求级 TPOT、ITL | 结果反馈和保护线 | 按运行规格、长度桶和流量等级观察，不由单个慢请求触发无界扩容 |
| KV cache 占用、preemption/recompute、OOM | 饱和度/诊断 | 区分活跃 KV 与可回收缓存；高显存或高 GPU busy 本身不证明过载 |
| Pending、Warming、Ready、Draining、容量批准量 | 供应与执行反馈 | 不把创建出的 Pod 数当成已提供的服务能力 |

基础度量维度包括 cluster、deployment UID、revision、InferencePool、role、runtime/hardware profile；SLO 再增加受控数量的 service class 和长度桶。共享服务统计所有有权调用项目的真实需求，硬件仍归 hosting project。

禁止把 prompt、API key、request_id 放进 Prometheus 标签。池隔离需要验证真实 scrape labels，不能只按 model_name 聚合，因为同一模型可能有 stable/canary 和多个项目的独立部署。

每个请求在一个观测时刻只能计入一次总需求。EPP 的 running 可能包含已送往 runtime 但仍等待的请求，不能再与 runtime running/waiting 简单相加。多 EPP 独立计数可以求和，共享镜像/HA 重复抓取则必须去重；无从证明时不启用总量驱动规则。

### 3.3 聚合、低流量与观测缺失

- 先对兼容 histogram buckets 聚合再计算分位数，不能平均各 Pod 的 p95；不混合 measured 与 predicted 样本。
- 采用短窗口发现突发、长窗口确认稳定，记录样本数、时间戳和 scrape 健康。建议初始实验用 1 分钟短窗、5 分钟长窗，不作为所有模型的生产默认。
- 低样本时 p99 不可靠，依靠队龄/并发/请求工作量与暖副本下限；不能因分位数缺失缩容。
- 指标 stale、采集失败与“健康且零流量”是不同状态。缺数默认禁止缩容；若可信需求信号完整，可以受限扩容，否则保持并告警。
- 错误、取消、容量拒绝和未完成长请求必须另行统计；只看成功完成样本会产生幸存者偏差。端到端 SLO 不得通过剔除过载失败来达标。

## 4. 策略：按需求提前扩，按瓶颈校正，验证有余量再缩

### 4.1 每种运行规格都需要容量定标

CapacityProfile 绑定模型权重/revision、runtime 版本、量化、TP/PP、GPU flavor、上下文/输出长度桶、batching 参数和缓存冷热条件。它记录在目标 SLO 下的安全请求率、并发范围、prefill/decode 能力以及启动时长分布。

QPS 相同，短问答与长上下文推理的成本可以完全不同。输入/输出 token 也不是统一线性成本；多模态、缓存命中和长上下文需要独立校准。未知输出长度采用按类别验证过的估计和不确定性余量，不把 max_tokens 当成已知真实输出量。

不能把“1 卡峰值 tokens/s”当作“1 卡能在 SLO 下长期承载的吞吐”。配置变更后 profile 失效或降置信度；先 shadow 校准再允许激进缩容。初版可使用压测得到的保守阈值，预测模型不是首版必需自研组件。

### 4.2 推荐扩容决策

1. 先验证需求合法、指标新鲜、profile 匹配，按路由到目标版本的需求计算；不要因已经被容量门禁挡住而把全部未满足需求隐藏。
2. 用到达率/长度分布、总在途工作和队列得到需求建议。多个建议是对同一容量需求的估计，按已验证组合规则取最紧约束，不直接相加重复算人头。
3. 延迟接近 SLO 且有队列/并发饱和证据时提前增加余量；实际突破 SLO 用于加速响应和告警，不是唯一触发点。
4. 计入已经准入并正常启动的在途副本，计算尚未覆盖的需求，避免每个采样周期重复追加同一批容量。
5. 输出有界的 recommendedReplicas，经 maxReplicas、变更速率和资源授权约束，进入 ModelDeployment 期望链路；记录不足的容量缺口。
6. 新副本 ServingReady 后观察 SLO 是否改善。没有改善且证据不支持容量瓶颈时停止同因的反复扩容，标记需要诊断；不能自动降回仍承载流量的副本。

这里是平台策略合同，不声称一个固定公式能预测所有模型的延迟。具体控制律优先复用上游，经 shadow/压测定标；不得直接使用 `新副本数 = 当前副本数 × TTFT/SLO` 当作通用容量定律。

### 4.3 不同异常，对应不同动作

| 现象 | 优先判断 | 推荐动作 |
| --- | --- | --- |
| 流量上升、队龄增加，TTFT 尚未超标 | 未来容量缺口 | 提前扩容，不等超标 |
| TTFT 高，runtime 排队/prefill 时间同步升高 | prefill/排队瓶颈 | 增加兼容服务容量；P/D 模式优先评估 prefill |
| TPOT/ITL 高、decode 活跃序列过多 | decode 饱和 | 增加 decode 容量、限制新增并发；不能迁移/加速已执行请求作默认承诺 |
| 入口 TTFT 高，但 runtime 与 EPP 均健康 | 鉴权、网络、代理或其他上游延迟 | 排查对应链路，不盲目扩 GPU |
| 单个超长请求慢，其他请求和队列正常 | 单请求计算下限或长度策略 | 独立服务等级、上下文/输出限制、硬件或模型配置评估 |
| 高 GPU busy，但 TTFT/TPOT 达标且无积压 | 有效批处理，也可能仍有容量 | 不仅凭 busy 扩容 |
| KV 紧张且频繁重算/抢占 | 活跃上下文容量不足 | 减并发/扩容；核对长度与缓存策略 |
| 新卡/新 Pod 一直 Pending | 授权或物理供应不足 | CapacityBlocked，保护现有服务，不制造更多无界 Pending |

这些是待验证的诊断规则，不是纯靠一项指标就能证明的因果关系；保留 trace 与决策证据用于排查。

### 4.4 缩容比扩容更严格

只有同时满足下列条件，才提出缩容：

- 需求与 SLO 在长窗口持续有余量，采集健康；无积压增长、持续容量拒绝或无法解释的超时。
- profile 支持缩容后的容量估计；评估负载重分布、缓存丢失与故障域，不只判断“这个 Pod 当前空闲”。
- 不低于 approved protectedReplicas、minReplicas 和已批准的定时暖容量；minReplicas 是期望下限，不能替代资源池的保障授权。
- 没有正在扩容/发布/回收的冲突动作，或协调状态机明确允许继续；已超时的失败启动单独处理。
- 执行上一份 RFC 的停止新派发、在途排空、真实释放协议。HPA 下调期望不代表 Operator 可以立即删除正在生成的实例。

建议压测初始配置：扩容以 15–30 秒级采样/决策观察趋势；缩容稳定窗口从 5–10 分钟开始，每次最多撤一个副本，再观察。大型模型可能需要更长窗口；小池可采用每 60 秒最多新增一个副本的保守起点。参数都需定标，不能直接照搬上游某种 GPU 的实验值。

## 5. 冷启动、预热与资源池借用

启动耗时拆成：等准入、等节点放置、拉镜像、取模型权重、加载/编译、运行探测、EPP/路由可用。分别观测，而不是只有一个 Pod Ready 时间。

**in-flight credit 只表示未来供应，不是当前可接流量的容量。** 已获准且进展正常的 Warming 副本用于抑制重复扩容；未准入 Pending、ImagePullBackOff、反复 OOM、超时启动不能无限折抵需求。记录启动截止时间、失败原因、重试预算和仍未满足的需求。

如果新副本 p95 启动耗时 120 秒，而 TTFT 目标为 1 秒，流量来后再扩容不可能挽救这 120 秒内的所有请求。必须根据业务峰值预留暖副本，并结合模型缓存、预拉镜像和可预测流量提前扩容。这是示例，不是项目实测数据。

预测/定时扩容采用“预测启动完成时刻的需求”，预测窗口至少考虑已测启动时间。预测失准只影响可借弹性部分，不能绕过授权；定时加暖也必须预先获得资源预算。突发保护依靠有界队列、租户公平、容量拒绝/重试策略，不承诺无限排队。

与资源池的关系：

- `recommendedReplicas` 表示无供应约束的需求建议；`desired/admitted/ready` 分别表示期望、准入与可服务容量，界面和告警不得混淆。
- HPA 推荐扩容不代表会获得 GPU。池满时保持可解释的缺口，暂停同因无效增长；借用仲裁仍由 Kueue 等准入后端决定。
- 借用回收与自动扩容共享动作状态机。回收时冻结受影响弹性组的新增准入，不能被 HPA 立刻补回；保障组继续服务。预测需求仍保留，不将冻结伪装成需求消失。
- 物理池扩容可作为管理员建议/后续基础设施集成；未经授权不自动购买 GPU 或增加云成本。
- scale-to-zero 非首版生产默认：需独立验证唤醒队列、超时、幂等、失败重试和冷启动 SLO。仅有服务内指标在零副本时无法唤醒，必须有外部需求信号。

## 6. 第三方选型与唯一写入者

### 6.1 首选验证：llm-d EPP + Prometheus + KEDA/HPA

llm-d 提供以 EPP 队列/活跃请求为信号的 KEDA 接入路径，并有 TTFT/TPOT 驱动的 SLO-aware 方案。该能力可以复用，不需要在 Pingora 重写 autoscaler。参见 [llm-d Autoscaling](https://llm-d.ai/docs/architecture/advanced/autoscaling)。

拟新增互斥模式：`manual`、`native-hpa`、`keda-epp`；高级 WVA 模式留待专项验证。

| 模式 | Operator 管理 | 其他控制器管理 | 副本期望写入者 |
| --- | --- | --- | --- |
| native-hpa | ModelDeployment 子资源及自有 HPA | Kubernetes HPA controller | 唯一 HPA 写 ModelDeployment /scale |
| keda-epp | ModelDeployment 子资源及自有 ScaledObject | KEDA 创建/维护 HPA | KEDA 管理的唯一 HPA 写 ModelDeployment /scale；零到一暂不开启 |
| manual | ModelDeployment 子资源，无 HPA/ScaledObject | 无自动扩缩 | 已授权人工操作 |

KEDA 可作用于实现 `/scale` 的自定义资源，但 XScope 的状态、selector、RBAC 与生命周期仍需实测，不能把上游直接 scale Deployment 的清单原样安装。参见 [KEDA Custom Resource Scaling](https://keda.sh/docs/2.18/concepts/scaling-deployments/)。

切换模式采用冻结动作、记录期望、删除/停用旧管理器并确认、创建新管理器的过程；不让两个 HPA 短暂长期竞争。Operator 不接管/覆写 KEDA 所有的 HPA，也不把子 Deployment 改成新的 scale target。外部资源归属冲突仍拒绝。

KEDA Prometheus scaler 直接查询 Prometheus，再发布 external metrics，不需要额外安装 Prometheus Adapter 才能走此链路。EPP 队列和活跃量是需求来源，但其 label 与作用域需要现场校验。参见 [llm-d KEDA with EPP Metrics](https://llm-d.ai/docs/architecture/advanced/autoscaling/hpa-epp)。

### 6.2 接入上游控制律时的约束

llm-d 的 SLO-aware 方案使用延迟相对目标的饱和信号、滞回及预热中的供应折抵。我们优先复用其可配置机制，不直接复制示例数值。参见 [SLO-aware KEDA control law](https://llm-d.ai/docs/architecture/advanced/autoscaling/slo-aware-keda)。

本平台额外要求：

1. 示例中的 P90 不等于客户的 P95/P99 合同；配置必须与承诺一致，样本不足时明确降级，不能悄悄换统计口径。
2. predicted latency 只能作为扩容建议，实际服务 SLO 始终由实测评估；预测器失准回退可信实测/需求策略。
3. 不将 NaN、断采与零流量合并；不以 `or vector(0)` 掩盖指标失败。Prometheus scaler 应显式处理空结果，例如评估 `ignoreNullValues=false`，同时另做时间戳与健康验证。参见 [KEDA Prometheus scaler](https://keda.sh/docs/2.18/scalers/prometheus/)。
4. 弄清输出单位：总需求的 AverageValue 目标是每副本承载量；已计算出的绝对副本建议不能再次乘当前 replicas。为 metricType/target/组合公式编写固定输入输出测试。
5. 多指标不是“每项都触发才扩容”。HPA 通常取各指标建议最大值，缺指标时有缩容保护；本设计复杂的置信度、冷启动和供应限制仍需适配验证，不假设 HPA 自动完成。参见 [HPA 多指标算法](https://kubernetes.io/docs/concepts/workloads/autoscaling/horizontal-pod-autoscale/)。
6. 健康但无流量时可按慢缩策略回到暖副本下限；指标不健康时 hold，不套用一个更小的固定 fallback 副本数造成意外缩容。

上述保护优先以 Prometheus recording rules、KEDA 配置和现有 Operator 生命周期实现。确需补业务状态适配时使用 Rust/kube-rs，而不是新增一套通用扩缩控制器。XScope 自有服务保持 Rust，第三方标准组件按固定版本使用，不为了统一语言移植它们。

### 6.3 WVA 与未来 P/D 分离

异构 GPU、多运行规格、prefill/decode 分离时，可进一步评估 llm-d WVA。官方资料将部分 token/SLO 分析能力标为实验性，并提示旧 VariantAutoscaling CRD 路径弃用；不能把旧清单或开发文档当作稳定部署合同。参见 [WVA Metrics](https://llm-d.ai/docs/architecture/advanced/autoscaling/hpa-wva)。

WVA 在本平台只能提供经过授权范围约束的需求/变体建议，不能覆盖 Kueue 的租户公平与准入结果。需要验证它对 ModelDeployment scale target、受限库存、protected/elastic 两组和 ownerReference 的适配；通过前维持同构单池方案，不引入双重资源仲裁。

P/D 分离需分别建立 role 的 CapacityProfile、队列和副本目标：TTFT 结合 prefill/入口等待定位，TPOT/ITL 结合 decode/传输定位；KV 传输、网络或两阶段不平衡不能靠同时扩大两组解决。共享请求流量不能在两个阶段重复计为两份独立业务需求，资源消耗则分别记账。跨节点多卡的缩放单位可能是完整 serving replica/group，不是随意增删一张卡。

## 7. 策略 API 与可观测状态草案

建议 SLOPolicy 与 CapacityProfile 为版本化业务合同，由 ModelDeployment 引用，仍存 PostgreSQL/SeaORM；内部 Kubernetes 投影由现有 agent/Operator 负责。业务维护者可在授权范围选经过验证的 profile 和 SLO 等级，不能提交任意 PromQL、Prometheus URL、优先级或借用策略。

下面仅是评审示例，不是已有 CRD/API，也不是生产推荐参数；数值须按真实模型压测调整：

```yaml
autoscalingPolicy:
  mode: keda-epp
  sloPolicyRef: interactive-standard-v1
  capacityProfileRef: model-a-a100-tp1-runtime-v1
  minReplicas: 2
  maxReplicas: 8
  protectedReplicas: 2
  demandSignals: [arrival-work, epp-queue, active-requests]
  feedbackSignals: [runtime-ttft, request-tpot]
  scaleUp: {observationWindow: 60s, maxAdditionalReplicasPerMinute: 1}
  scaleDown: {stabilizationWindow: 600s, maxRemovedReplicasPerStep: 1, drainRequired: true}
  missingMetrics: Hold
  scaleToZero: false
sloPolicy:
  serviceClass: interactive
  ttft: {boundary: platform-ingress, percentile: 95, target: 1s}
  tpot: {boundary: runtime-request, percentile: 95, target: 50ms}
  minSamplesForLatencyFeedback: 100
  lowSampleBehavior: DemandSignalsOnly
```

入口 TTFT 预算要为代理/网络/排队留余量；runtime 反馈阈值从 profile 与预算分解中生成，不能默认令入口目标等于 runtime 阈值。所有公开 SLO 均需说明适用的请求长度、输出形式与成功/拒绝规则。

状态至少包含：policy/profile revision、指标时间/样本数、推荐/期望/准入/Ready 副本、Warming/Draining 数量、上次决策与触发证据、下一次允许动作时间。原因码包括 `DemandRising`、`LatencyPressure`、`WarmupInFlight`、`MetricsStale`、`InsufficientSamples`、`CapacityBlocked`、`ProfileMismatch`、`ReclaimInProgress`、`NonCapacityBottleneckSuspected`。

记录扩缩事件及关联 grant generation，但不把每次 15 秒评估都写成一条账务事件。指标持续采集，状态变化/关键决策才写审计，控制低 CPU/数据库开销。

## 8. 实施与验收门槛

按主 RFC 阶段推进：阶段 B 明确合同和唯一 HPA 所有权并做指标适配验证；阶段 C 补排空/取消/事件恢复；阶段 D 补多集群状态可靠性；阶段 E 完成 SLO 闭环和生产 GPU 压测。基础指标验证是开发前提，不等待完整 SLO 产品 UI 才开始；UI 仍在后端验证之后。

验证顺序：`指标合同 → 录制流量回放/shadow 建议 → 只允许受限扩容 → 开启安全缩容 → 借用联动 → 预测/P-D/异构优化`。初期 shadow 不改副本，并与静态容量和传统 HPA 基线对照；失败可切回已验证固定副本且保留准入门禁。

| 测试 | 通过标准 |
| --- | --- |
| 阶跃/渐增/周期流量 | 能提前或及时识别需求；记录检测到 Ready 的时间及 SLO 违约面积 |
| 相同 QPS，短输入变长输入、短输出变长输出 | 不把 QPS 不变误判为负载不变；定位 TTFT/TPOT 的变化 |
| 冷缓存、热缓存与路由调整 | profile/置信度生效；扩容导致缓存变冷不引发无限追加 |
| 新增副本预热两分钟、部分启动失败 | 正常 Warming 只抵扣一次未来需求；失败供应不被长期信任 |
| 无完成样本但队列增长 | 不因 TTFT/TPOT 缺失当作空闲；通过需求信号告警/受限扩容 |
| 断采、NaN、低样本、跨池同名模型、多 EPP | 不意外缩容、不串池、不重复计数 |
| 单长请求、慢客户端、代理延迟故障 | 不因非 GPU 瓶颈反复扩 GPU |
| 达到 hardMax、池满、借用回收 | 显示缺口与原因，不越权；回收与 HPA 不互相补副本 |
| 稳定/灰度切流，低流量 canary | 分版本指标/暖容量，切流前准备容量；样本不足不凭空宣称 SLO 达标 |
| 缩容中的长 SSE、取消、进程重启 | 遵守排空协议，重复事件不重复收费或释放资源 |
| native-hpa ↔ keda-epp 切换 | 任一时刻不出现两个有效扩缩写入者，旧 owner 不被接管 |
| P/D 或异构扩展 | 另行验证瓶颈归因、资源边界、伸缩单位；不以同构结果替代 |

验收报告同时比较 Goodput/GPU-hour、p95/p99 TTFT、请求级 TPOT、ITL、错误/拒绝/取消率、实际 GPU/CPU/内存成本、扩缩次数和预热浪费。固定数据集、同一请求到达轨迹、相同硬件与模型参数，报告适用负载范围，而不是仅展示 GPU busy 上升。

具体延迟和容量达标阈值需业务确认。CPU/Echo 可验证控制链和状态，无法替代真实 GPU 上的以上性能验收。本设计未执行这些试验，也不承诺仅靠自动扩缩容能够无条件满足所有突发流量的 SLO。
