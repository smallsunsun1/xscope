# XScope

XScope 是一个面向大模型 API 的云原生服务平台。源码按语言组织，每个语言目录都是可被 IDE 和原生工具直接识别的工作区；Bazel/Bzlmod 仍是整个 monorepo 的统一构建入口。

## 代码布局

| 目录 | 工具链 | 职责 |
| --- | --- | --- |
| `rust/gateway` | Cargo workspace、Pingora 0.8、Redis、rules_rust | 鉴权、全局 RPM/TPM、加权路由、HTTP 代理、usage WAL |
| `rust/services/control-plane` | Axum、SeaORM、PostgreSQL、rules_rust | 身份/租户、项目/API Key、计量、订单、支付退款、账本、发票和管理 API |
| `rust/crates/{domain,entities,migration}` | Rust workspace、SeaORM 2 | 业务领域、数据库实体和版本化迁移 |
| `go/cluster-agent` | Go module、chi、controller-runtime、rules_go | 控制面与成员 Kubernetes 集群之间的窄权限 API |
| `go/operator` | controller-runtime、Kubernetes API | 将 `ModelDeployment` 协调为 Deployment/Service 并回写状态 |
| `go/api` | Kubernetes API types | `platform.xscope.io/v1alpha1` 类型定义 |
| `python` | pyproject、FastAPI、Pydantic | OpenAI Chat Completions 开发 runtime |
| `web` | React、Ant Design、TanStack Query、Vite、rules_js | 管理控制台 |
| `api` / `deploy` | OpenAPI、JSON Schema、YAML | 对外契约和 Kubernetes 清单 |

推荐直接打开 `xscope.code-workspace`。它已经为 rust-analyzer、gopls、Python/Pylance 和 TypeScript 配置了各自的项目入口。Cargo、`go.mod`、`pyproject.toml` 是 IDE 和语言工具的依赖事实来源，Bazel 从这些锁文件导入依赖并统一编排。

## 开发

构建入口统一为 Bazel 9.2.0；各语言工具链由 Bazel 下载。macOS 本机构建需要系统 C/C++ 工具链。部署镜像在 Linux Bazel 构建环境中通过 rules_oci 产出，Docker 只负责运行构建环境和导入镜像归档。

```bash
# 整个 monorepo
bazel build //...
bazel test //...

# 局部测试也使用 Bazel
bazel test //rust/... //go/... //python:runtime_test

# Web：安装 IDE 可见的 node_modules、检查并构建可部署产物
bazel run -- @pnpm//:pnpm --dir "$PWD/web" install --frozen-lockfile
bazel test //web/console:typecheck
bazel build //web/console:console
```

Bazel 依赖映射：Go 由 Gazelle 读取 `go/go.mod`，Rust crate_universe 读取 `rust/Cargo.toml` 和 `rust/Cargo.lock`，Python pip hub 读取 uv 生成的 `python/requirements.lock`，Web 由 rules_js 读取 `web/pnpm-lock.yaml`。前端依赖变化后，用 Bazel 管理的 pnpm 更新锁文件：

```bash
bazel run -- @pnpm//:pnpm --dir "$PWD/web" install --lockfile-only
```

### 管理控制台

控制台包含平台总览、项目、API Key 的 RPM/TPM/模型/预算策略、模型价格、用量、余额门禁、充值订单、支付退款、双分录账本、发票、对账，以及 Kubernetes `ModelDeployment` 管理。控制面未连接成员集群或 CRD 未安装时，部署页会进入只读保护态，其余管理功能仍可使用。

分别启动控制面和前端开发服务器：

```bash
bazel run //rust/services/control-plane

# 在仓库根目录的另一个终端
bazel run -- @pnpm//:pnpm --dir "$PWD/web" dev
```

打开 `http://127.0.0.1:5173`。生产静态文件位于 `bazel-bin/web/console/dist`。VS Code 补全依赖 `web/node_modules`，首次拉取代码后执行上面的 frozen-lockfile 安装命令即可。

启动开发 runtime 后，用 JSON 注入网关 Key 和 endpoint：

```bash
bazel run //python:runtime

XSCOPE_API_KEYS_JSON='[{"id":"key-local","tenant_id":"tenant-local","project_id":"project-local","secret":"xscope-local-secret"}]' \
XSCOPE_UPSTREAMS_JSON='[{"id":"runtime-dev","address":"127.0.0.1:8090","weight":100}]' \
bazel run //rust/gateway

curl http://127.0.0.1:8080/v1/chat/completions \
  # Authorization credentials are supplied from a runtime secret.
  -H 'Content-Type: application/json' \
  -d '{"model":"xscope-demo","messages":[{"role":"user","content":"hello"}]}'
```

Pingora 在内存中只保留 Key 的 SHA-256 摘要，并校验 Scope、允许模型、过期时间、月预算和预付余额。网关从控制面的内部接口轮询策略并原子替换 last-known-good 快照；静态 Key 仅作为启动回退。多网关副本通过 Redis Lua 原子预占 RPM/TPM，并在响应后按真实 token 回补。通过策略校验的请求会先把 usage 写入 append-only WAL，再异步上报；Kubernetes 部署中的 WAL 位于持久卷 `/var/lib/xscope/usage-wal/events.jsonl`。Rust 控制面通过 SeaORM 以 `event_id` 幂等写入 PostgreSQL，并生成精确到微分单位的双分录。

Kubernetes Secret 的 `keys.json` 字段使用同一个 JSON 格式：

```bash
kubectl -n xscope-system create secret generic xscope-gateway-keys \
  --from-literal=keys.json='[{"id":"key-local","tenant_id":"tenant-local","project_id":"project-local","secret":"replace-this-secret"}]'
```

### Docker Desktop 一键部署

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

Docker Desktop 本地初始凭据仅用于开发：控制台用户 `platform-admin` / `xscope-local-admin`，
Keycloak 管理员 `admin` / `xscope-local-keycloak-admin`，推理 API Key
`xscope-local-secret`。这些值位于 `deploy/k8s/overlays/local/secrets.yaml`，不得用于共享或
生产集群。

网关统一使用 `tracing`；本地 Kubernetes overlay 输出 JSON 日志。可通过 `RUST_LOG` 调整
target 过滤规则，并通过 `XSCOPE_LOG_FORMAT=compact|json` 选择输出格式。

本地部署同时携带 `XSCOPE_CLUSTER_ID=docker-desktop` 与 `XSCOPE_REGION=local`。Operator
会把它们写入模型工作负载标签和 `ModelDeployment.status`，为后续把数据面独立安装到多个
推理集群保留稳定身份。完整拓扑和扩展约束见 `docs/multicluster.md`。

Operator 必须运行在集群内或具备有效 kubeconfig：

```bash
bazel run //go/operator/cmd/operator
```

## 垂直切片进度

1. 已完成：Rust/Axum/SeaORM 控制面、PostgreSQL 账号与租户成员、项目/API Key、模型目录和报价，以及 Ant Design 管理台。
2. 已完成：Rust 控制面经 Go cluster-agent 创建/扩缩/删除 `ModelDeployment`；Go Operator 只负责 Kubernetes reconcile。
3. 已完成初版：Pingora 执行 Scope、模型、过期、月预算、余额门禁；Redis Lua 在所有网关副本间执行 RPM/TPM 预占与结算。
4. 已完成初版：usage 持久化 WAL、至少一次上报、数据库幂等去重、精确费用、充值/支付/退款、双分录、发票记录和渠道对账。
5. 按顺序推进：SSE/取消 → InferencePool/llm-d EPP → RoutePolicy/stable-canary → HPA/PDB/资源所有权 → 计费预占/WAL checkpoint/事件流 → 多集群 → 可观测性/审计 → 正式支付税务与更多推理 API。验收状态见 `docs/implementation-sequence.md`。

## 工程约定

- Bazel/Bzlmod 是统一 CI/发布构建入口；语言自身的 manifest 与 lockfile 是依赖事实来源，也保留原生 IDE、测试和本地反馈链路。
- 公共边界用 OpenAPI、JSON Schema，后续内部高频 RPC 再引入 Protobuf/gRPC。
- 金额使用最小货币单位整数，token/请求量使用整数；usage event 只追加、不原地更新。
- 所有资源都携带 `tenant_id`、`project_id`、`region`，所有写接口接受幂等键。
