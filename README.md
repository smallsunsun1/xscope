# XScope

本地自动缩容增量已部署：受管 v2 入口通过真实 KEDA 升降副本、SSE 排空和 Gateway 容器终止证明验证；保留原入口和历史数据。配置、边界与 Bazel 运维入口见 [自动缩容与终止证明](docs/safe-autoscaling.md)。演示使用 Echo，不是 GPU 模型；节点失联等缺证据情况仍保持阻塞。

2026-09-12 开发增量：新增 [受管 Pool 自动注册、Gateway ACK、排空和 UID 手动缩容](docs/managed-traffic.md)，只适用于独占受管入口。代码尚未部署，不将离线视为零在途，也未开启 KEDA 自动缩容。

2026-09-11 开发增量：新增数据库模型目录与不可变价格版本、Gateway 请求体多模型路由、发布暂停/提升/历史回滚，以及 Runtime 可配置启动就绪探针。接口和测试见 [模型目录与发布](docs/model-catalog.md)。本次代码尚未部署，自动注册与安全排空等剩余规划不视为完成。

2026-09-06 用量投递更新：Gateway 仅采用 **HTTP + 有界内存重试**，不依赖 SQLite/WAL/PVC；控制面仍通过 SeaORM/PostgreSQL 幂等记账。未知用量新增双人审核损失豁免，绝不自动超时解冻。配置、故障边界、测试与旧 WAL 安全切换见 [轻量投递方案](docs/usage-delivery.md)。旧集群首次部署请先阅读切换步骤，不能用删除历史 PVC 的方式迁移。

项目级灰度路由已接入：本地控制台 [流量路由](http://localhost:30081/#/routing) 支持请求头规则、Stable/Canary 权重和版本冲突保护。使用方式及当前 echo 模型限制见 [RoutePolicy 指南](docs/route-policy.md)。

模型弹性采用 KEDA ScaledObject 单入口，Operator 不再直接生成 HPA；GPU/SLO 策略、资源池准入和安全缩容按阶段推进。当前实现范围、安装及验证见 [KEDA 接入](docs/keda-autoscaling.md)。

XScope 是一个面向大模型 API 的云原生服务平台。源码按语言组织，每个语言目录都是可被 IDE 和原生工具直接识别的工作区；Bazel/Bzlmod 仍是整个 monorepo 的统一构建入口。

## 代码布局

| 目录 | 工具链 | 职责 |
| --- | --- | --- |
| `rust/crates/gateway` | Rust workspace、Pingora 0.8、Redis、rules_rust | 鉴权、全局 RPM/TPM、HTTP/SSE 代理、取消传播、有界 HTTP 用量上报 |
| `rust/crates/control-plane` | Axum、SeaORM、PostgreSQL、rules_rust | 身份/租户、项目/API Key、计量、订单、支付退款、账本、发票和管理 API |
| `rust/crates/{domain,entities,migration}` | Rust workspace、SeaORM 2 | 业务领域、数据库实体和版本化迁移 |
| `rust/crates/cluster-agent` | Axum、kube-rs | 控制面与成员 Kubernetes 集群之间的窄权限 API |
| `rust/crates/operator` | kube-runtime、kube-leader-election | 将 `ModelDeployment` 协调为 Deployment/Service，Lease 选主与状态回写 |
| `rust/crates/kubernetes` | kube-rs、k8s-openapi | 兼容现有 CRD 的类型、校验与资源协调逻辑 |
| `rust/crates/telemetry` | tracing、OpenTelemetry、Prometheus | Rust 服务共享的日志、追踪和指标 |
| `python` | pyproject、FastAPI、Pydantic | OpenAI Chat Completions 开发 runtime |
| `web` | React、Ant Design、TanStack Query、Vite、rules_js | 管理控制台 |
| `api` / `deploy` | OpenAPI、JSON Schema、YAML | 对外契约和 Kubernetes 清单 |

推荐直接打开 `xscope.code-workspace`。全部 Rust package 位于 `rust/crates/`，workspace 根清单位于 `rust/Cargo.toml`，保留给 IDE 和 Bazel 依赖解析使用。应用编译、测试与镜像打包仅使用 Bazel，不再包含 Go 源码或 Go module。

首次打开或修改依赖后，运行 `bazel run @rules_rust//tools/rust_analyzer:gen_rust_project -- //rust/crates/...`，再重载 rust-analyzer。[工作区和迁移说明](docs/rust-workspace.md)。

## 开发

构建入口统一为 Bazel 9.2.0；各语言工具链由 Bazel 下载。macOS 本机构建需要系统 C/C++ 工具链。部署镜像在 Linux Bazel 构建环境中通过 rules_oci 产出，Docker 只负责运行构建环境和导入镜像归档。

```bash
# 整个 monorepo
bazel build //...
# 首次运行完整测试前，先安装 Playwright 对应的 Chromium
bazel run //web/console:install_browsers
bazel test //...

# 局部测试也使用 Bazel
bazel test //rust/... //python:runtime_test

# Web：安装 IDE 可见的 node_modules、检查并构建可部署产物
bazel run -- @pnpm//:pnpm --dir "$PWD/web" install --frozen-lockfile
bazel test //web/console:typecheck
bazel build //web/console:console
```

Bazel 依赖映射：Rust crate_universe 读取 `rust/Cargo.toml` 和 `rust/Cargo.lock`，Python pip hub 读取 `python/requirements.lock` 和 Bazel 锁定的 `python/telemetry.lock`，Web 由 rules_js 读取 `web/pnpm-lock.yaml`。前端依赖变化后，用 Bazel 管理的 pnpm 更新锁文件：

```bash
bazel run -- @pnpm//:pnpm --dir "$PWD/web" install --lockfile-only
```

### 管理控制台

右上角支持简体中文 / English 切换，并记住语言偏好。运行 `bazel test //web/console:checks` 可执行类型、金额格式及 Playwright 浏览器回归检查；测试范围与 CI 配置见 [前端质量检查](docs/frontend-quality.md)。

控制台包含平台总览、项目、API Key 的 RPM/TPM/模型/预算策略、模型价格、用量、余额门禁、充值订单、支付退款、双分录账本、发票、对账，以及 Kubernetes `ModelDeployment` 管理。控制面未连接成员集群或 CRD 未安装时，部署页会进入只读保护态，其余管理功能仍可使用。

分别启动控制面和前端开发服务器：

```bash
bazel run //rust/crates/control-plane

# 在仓库根目录的另一个终端
bazel run -- @pnpm//:pnpm --dir "$PWD/web" dev
```

打开 `http://127.0.0.1:5173`。生产静态文件位于 `bazel-bin/web/console/dist`。VS Code 补全依赖 `web/node_modules`，首次拉取代码后执行上面的 frozen-lockfile 安装命令即可。

仅做本机传输调试时，可直接启动开发 runtime，并把它作为测试 serving 入口（不经过 EPP；集群部署使用下面的 InferencePool 链路）：

```bash
bazel test //rust/crates/gateway:gateway_test //tools:streaming_e2e_test
# 已部署的本地集群：运行时从 Kubernetes Secret 读取测试 Key，不打印它。
bazel run //tools:inference_cluster_smoke
```

Pingora 在内存中只保留 Key 的 SHA-256 摘要，并校验 Scope、允许模型、过期时间、月预算和预付余额。网关从控制面的内部接口轮询策略并原子替换 last-known-good 快照；静态 Key 仅作为启动回退。多网关副本通过 Redis Lua 原子预占 RPM/TPM，并在响应后按真实 token 回补。资金模式先通过控制面的事务预占/派发，再转发推理；完成后由有界内存队列通过 HTTP 上报。Rust 控制面以 reservation/payload（开发 v1 接口以 `event_id`）幂等写入 PostgreSQL 并生成精确双分录。Gateway 强杀可能丢失未确认用量，中央冻结不自动释放，见 [故障边界](docs/usage-delivery.md)。

推理请求支持 `stream: true`，逐块透传 SSE，并在客户端断开时取消上游请求。计量解析使用 `sse-core`，不拼接整段输出；未知 usage 不再作为 0 token 成功请求处理。协议、测试和计费边界见 [流式推理说明](docs/streaming.md)。

集群推理链路为 Pingora → `inference-serving`（Envoy + llm-d EPP）→ `InferencePool/demo-pool` 中的 Runtime Pods。Pingora 不再维护模型 Pod 列表或选择模型副本。路由部署、资源边界与验证步骤见 [InferencePool 接入说明](docs/inference-serving.md)。

Kubernetes Secret 的 `keys.json` 字段使用同一个 JSON 格式：

```bash
kubectl -n xscope-system create secret generic xscope-gateway-keys \
  --from-literal=keys.json='[{"id":"key-local","tenant_id":"tenant-local","project_id":"project-local","secret":"replace-this-secret"}]'
```

### Docker Desktop 一键部署

已安装环境的最新[可靠性与运维扩展](docs/reliability-operations.md)提供有界计费并发、每日业务快照、隔离恢复、站内告警和“运行与恢复”页面；该文档包含保留 live 配置的定向增量升级命令及生产边界。

本地 overlay 会把管理面和数据面共同部署到当前 `kubectl` context，常驻 CPU request
约为 175m（不包含用户创建的模型工作负载）。控制台由 OAuth2 Proxy 保护，账号由
Keycloak 完成 OIDC 登录，Rust 控制面把平台用户、租户成员关系、项目、Key、用量和财务账本通过 SeaORM 持久化到 PostgreSQL；前端静态文件由控制面进程直接提供，减少一个
常驻 Pod。

```bash
./tools/deploy-local.sh

# 仅编译和打包 Linux OCI 镜像（不更新集群）
./tools/bazel-linux.sh images

# 控制台 / 身份服务 / 推理入口
open http://localhost:30081
open http://xscope.localhost:30080
curl http://localhost:30082/healthz

# 登录后可直接查看实际用量与费用
open http://localhost:30081/#/billing

# 查看运行状态与 Rust 服务结构化日志
kubectl get pods -n xscope-system -w
kubectl logs -n xscope-system deployment/gateway -f
kubectl logs -n xscope-system deployment/control-plane -f
```

本地初始凭据在首次部署时随机生成并存入 Kubernetes Secret，后续部署保留现有值；仓库不保存可用密码或 API Key。控制台用户为 `platform-admin`，Keycloak 管理员为 `admin`。在本机受信任终端或 Kubernetes 管理工具中查看相应 Secret，勿把值复制到聊天、提交、截图或构建环境。详见 [仓库安全规则](docs/repository-security.md)。

Rust 服务统一使用 `tracing`；本地 Kubernetes overlay 输出 JSON 日志。可通过 `RUST_LOG` 调整
target 过滤规则，并通过 `XSCOPE_LOG_FORMAT=compact|json` 选择输出格式。
已部署低资源的 Jaeger、Prometheus 和 Grafana，三者使用独立 PVC 持久化，提供 OTLP
追踪、逐 Pod 指标采集和预配置看板；查看方式、
验证命令与尚未完成的边界见 [可观测性说明](docs/observability.md)。

本地部署同时携带 `XSCOPE_CLUSTER_ID=docker-desktop` 与 `XSCOPE_REGION=local`。Operator
会把它们写入模型工作负载标签和 `ModelDeployment.status`，为后续把数据面独立安装到多个
推理集群保留稳定身份。完整拓扑和扩展约束见 `docs/multicluster.md`。

Operator 必须运行在集群内或具备有效 kubeconfig：

```bash
bazel run //rust/crates/operator
```

## 垂直切片进度

1. 已完成：Rust/Axum/SeaORM 控制面、PostgreSQL 账号与租户成员、项目/API Key、模型目录和报价，以及 Ant Design 管理台。
2. Rust 控制面经 kube-rs cluster-agent 创建/版本化更新/扩缩/删除 `ModelDeployment`；独立 Rust Operator 管理 Runtime、HPA、PDB、InferencePool，并保留安装层 llm-d/EPP 所有权，业务控制面仍不持有 Kubernetes 凭证。配置、发布边界和验收命令见 [Operator 生命周期](docs/operator-lifecycle.md)。
3. 已完成初版：Pingora 执行 Scope、模型、过期、月预算、余额门禁；Redis Lua 在所有网关副本间执行 RPM/TPM 预占与结算。
4. 已完成初版：HTTP 有界重试上报、数据库幂等去重、精确费用、手工充值/支付/退款记录、双分录、发票记录和渠道对账记录。已接入中央未决证据、双人审核豁免、Redis 配额幂等恢复，以及 [Gateway 资金预占/派发/结算与事务 Outbox](docs/billing-protocol.md)；未知用量保留冻结，正式供应商证据对账和真实支付/税务渠道仍待接入，见 [可靠性边界](docs/billing-recovery.md)。
5. 按顺序推进：SSE/取消 → InferencePool/llm-d EPP → RoutePolicy/stable-canary → HPA/PDB/资源所有权 → 计费预占/HTTP 幂等上报/事件流 → 多集群 → 可观测性/审计 → 正式支付税务与更多推理 API。验收状态见 `docs/implementation-sequence.md`。

## 工程约定

- Bazel/Bzlmod 是开发、测试和发布的统一构建入口；语言自身的 manifest 与 lockfile 保留为依赖和 IDE 元数据，不再维护第二套原生构建路径。
- 公共边界用 OpenAPI、JSON Schema，后续内部高频 RPC 再引入 Protobuf/gRPC。
- 金额使用最小货币单位整数，token/请求量使用整数；usage event 只追加、不原地更新。
- 所有资源都携带 `tenant_id`、`project_id`、`region`，所有写接口接受幂等键。
