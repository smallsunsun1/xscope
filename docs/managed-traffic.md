# 受管 Pool：自动注册、ACK、排空与手动缩容

这是 2026-09-12 的代码增量，不代表当前 Kubernetes 已升级。Gateway 仍无本地持久状态：用量继续通过有界内存队列 HTTP 上报，流量许可和在途计数也仅保存在内存；控制面使用 SeaORM/PostgreSQL 保存参与者及排空证据，不恢复 WAL/SQLite。

## 前提与资源所有权

本流程仅适用于新建的、独占的受管入口，不追认旧路由的排空安全性。

- 成员使用出站 Agent 和独立集群凭据。它只管理批准的 namespace，不接收 kubeconfig 出站传输。
- Operator 继续拥有 ModelDeployment 的 Runtime Deployment、Service、InferencePool、PDB。Envoy/EPP Deployment、Service、配置和 RBAC 仍由安装层拥有，本次不接管或自动删除这些资源。
- 一个受管 Pool 绑定一个 ModelDeployment 和一个同 namespace、同名 Service/Deployment 的独立 Envoy/EPP 入口。当前只允许手动 replicas，不能同时启用 KEDA。
- 入口必须使用下面的受管 Envoy 配置，且 NetworkPolicy 必须限制服务端口只允许受信任的 XScope Gateway。安装者在入口 Service 上声明 `platform.xscope.io/inference-pool: <ModelDeployment 名称>` 和 `platform.xscope.io/managed-only: "true"`。
- 该声明是安装层对独占入口的承诺。Agent 不读取 Secret 配置或替管理员证明 NetworkPolicy 已被 CNI 执行；没有满足这些前提，不能宣称能安全缩容。Kubernetes 管理员直接修改工作负载也能绕过控制面流程。

```bash
bazel build //deploy/inference:managed_envoy
bazel run //deploy/inference:managed_envoy_check
```

第一个目标从已有基础配置生成 `envoy-managed.json`，不复制维护整份配置。新增 RBAC 过滤器在 EPP 之前拒绝缺少 `x-xscope-managed-pool: managed-*` 的请求。Gateway 先移除客户端控制头，只有通过本地流量许可后才注入此标记。标记不是独立凭据，不能代替网络隔离或 mTLS。配置仍继承基础入口的 Chat Completions 路由，不因此宣称更多推理 API 已通过完整 EPP 链路。

实际入口地址、配置和凭据使用 Secret；不要把私有值作为 Bazel 输入、放进仓库、日志或示例。本页所有名称仅为合成示例。

## 自动注册

先通过模型目录 API 建立价格和模型权限，再通过成员 desired-state API 创建 ModelDeployment。随后由平台管理员绑定入口：

```text
POST /admin/v1/managed-pools
GET  /admin/v1/managed-pools
GET  /admin/v1/managed-pools/{id}
```

绑定请求的形状如下；实际部署时从私有运行配置构造请求，不复制实际值到文档。

```json
{
  "cluster_id": "synthetic-cluster",
  "deployment": "synthetic-model-v1",
  "serving_service": "synthetic-serving-v1",
  "address": "synthetic-serving.invalid:8085",
  "expected_desired_version": 1,
  "make_default": true
}
```

可选 `tls`、`server_name` 控制入口传输。控制面生成不可复用的 `managed-*` ID，返回 `pending`；这时不接收推理。

| 标识 | 用途 |
| --- | --- |
| 模型目录 ID | 客户请求体里的 `model`，对应模型权限和价格 |
| ModelDeployment 名称 | 同时也是 Operator 管理的 InferencePool 名称 |
| `managed-*` ID | 控制面的路由 Pool 标识，不是 Kubernetes 对象名称 |

成员 Agent 获取有 generation/nonce 的观测任务，检查：

1. ModelDeployment 的集群标签、UID、已应用版本和实际 spec。
2. Runtime Deployment 的 owner、当前 observedGeneration，以及 updated/ready/available replicas。
3. Envoy/EPP Deployment 的就绪和 Service 选择器、独占声明。
4. InferencePool 的 owner、选择器、端口和 EPP 引用。

全部通过后自动注册为 `active`。`make_default=true` 只填充尚未绑定的模型默认 Pool，不替换已有 stable。模型尚未就绪时仍记录已确认的 UID，允许之后在无请求条件下排空并清理冷启动失败的部署。

观测 nonce 有 60 秒期限；generation 或对象身份变化的旧报告被拒绝。就绪证据有效 45 秒，过期停止发放新的接受许可。这里的“就绪”依赖引擎自己的 startup/readiness 契约，不是模型下载或 GPU warmup 的替代实现。

已知的旧入口地址/模型 revision 不能与受管入口混用，后续创建旧式别名也被拒绝。注册写入通过数据库行锁串行化，避免两个管理员并发绕过检查。不要通过外部 DNS 别名或手工配置规避独占入口约束。

## Gateway 许可与 ACK

每个 Gateway 进程启动时生成新的 incarnation ID，通过带 `session` 的快照请求获取 15 秒流量许可。续期最多间隔 5 秒，租期从发起请求时的单调时钟计算，慢响应不延长租期。旧的不带 session 的快照不暴露受管入口地址。

控制面在返回可接受流量的快照之前，先持久记录该 incarnation 曾获得该 Pool 的许可。即使响应丢失，这个参与者也不能从排空判断中消失。

Gateway 在同一把本地锁下安装关闭状态、检查准入并增加在途数。计数覆盖资金准入和完整 HTTP/SSE 生命周期；请求上下文结束才释放。旧请求上下文不能在新一代关闭状态下再次准入。租约过期只拒绝新准入，已在执行的请求继续排空。

ACK 在本地安装快照之后发送，包含实际在途数及快照 sequence/nonce。同一 sequence 只能重放同一报告，陈旧或更改内容的 ACK 被拒绝。

项目 owner 可查看策略版本 ACK：

```text
GET /admin/v1/projects/{project}/models/{model}/route-policy/acks
```

响应明确包含 `known_sessions_only=true`，不是对旧版、非协议参与者的全局证明。受管 Pool 的排空使用该 Pool 所有曾获许可的 incarnation 记录判断，不只观察当前活跃副本。

## 排空与手动缩容

状态流转：`pending → active → draining → drained → active`；已删除部署的绑定进入终态 `retired`。

```text
POST /admin/v1/managed-pools/{id}/drain
POST /admin/v1/managed-pools/{id}/finish-drain
POST /admin/v1/managed-pools/{id}/activate
```

三者都需要 `{"expected_generation":当前代数}`，并受平台管理员权限和审计保护。

1. 如需保持可用性，先把项目路由切到另一就绪 Pool 并观察 ACK。单 Pool 排空期间，新请求返回 503 是预期行为。
2. `drain` 增加 generation，停止新准入。已开始的 SSE 不强制中断。
3. 查询状态，等待所有相关 incarnation 确认关闭代数且在途数为零，再调用 `finish-drain`。不满足时返回 409。
4. 通过现有 `PUT /admin/v1/clusters/{id}/desired-state` 修改 replicas。绑定部署只有在 `drained` 时才能变更；此流程只允许 replicas 变化，新模型/runtime revision 必须创建新部署和 Pool。
5. 控制面自动附加已观察到的 `expected_uid`。Agent 同时检查 UID 和 Kubernetes resourceVersion，避免缩容到同名替换对象。
6. 缩容后继续保持 `drained`，新的就绪观测不会自动恢复流量。确认 fresh readiness 后，显式 `activate` 才重新发放许可。

删除同样要求先完成排空，且 `delete_uid` 必须匹配观测身份。删除后的 `retired` 绑定不可重新用于创建同名工作负载；安装层的 Envoy/EPP 不因此删除。

Gateway 正常退出会先关闭准入、结束请求，再提交不可撤销的零在途退休报告。该 incarnation 之后不能获取新许可，未来排空不必等待它。**进程崩溃、网络分区、旧时间戳或租约过期都不等于退休/零在途证明**；没有证据就继续阻止缩容。当前没有强制跳过接口，仍需后续接入可靠的 Pod/节点终止证明和事故恢复流程。

不要删除 `gateway_sessions`/`gateway_pool_views` 行来解除阻塞。数据库恢复到历史时间点也不能视为持有完整流量证据，必须先隔离数据面并进行恢复核查。

## 验证与边界

```bash
bazel test //... --jobs=3 --test_output=errors
bazel run //tools:billing_protocol_smoke --jobs=3
bazel run //deploy/inference:managed_envoy_check
```

数据库集成包含真正的控制面、Agent 和两个 Gateway 进程，但 Kubernetes API 与流式引擎是隔离 fixture。覆盖未就绪不注册、自动默认绑定、陈旧观测、在途 SSE、策略 ACK、暂停准入、响应丢失和离线阻塞、正常退休、UID 缩容及删除边界。Envoy 使用已固定版本的真实二进制验证生成配置，验证时禁用网络。

这些测试不等于真实 GPU、跨地域链路或第二真实集群验收。本轮没有更新在线 Kubernetes。KEDA 自动缩容、逐 Pod 无停机 drain、未知 incarnation 的终止证明、资源池/Kueue 准入仍未完成；不把受管 Pool 的手动缩容扩大为整个自动弹性系统已完成。
