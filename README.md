# XScope

XScope 是一个面向大模型 API 的云原生服务平台。源码按语言组织，每个语言目录都是可被 IDE 和原生工具直接识别的工作区；Bazel/Bzlmod 仍是整个 monorepo 的统一构建入口。

## 代码布局

| 目录 | 工具链 | 职责 |
| --- | --- | --- |
| `rust/gateway` | Cargo workspace、Pingora 0.8、rules_rust | 鉴权、加权路由、健康检查、HTTP 代理、usage WAL |
| `go/control-plane` | Go module、chi、validator、rules_go | 项目/API Key、模型目录、报价和管理 API |
| `go/operator` | controller-runtime、Kubernetes API | 将 `ModelDeployment` 协调为 Deployment/Service 并回写状态 |
| `go/api` | Kubernetes API types | `platform.xscope.io/v1alpha1` 类型定义 |
| `python` | pyproject、FastAPI、Pydantic | OpenAI Chat Completions 开发 runtime |
| `web` | React、Ant Design、TanStack Query、Vite、rules_js | 管理控制台 |
| `api` / `deploy` | OpenAPI、JSON Schema、YAML | 对外契约和 Kubernetes 清单 |

推荐直接打开 `xscope.code-workspace`。它已经为 rust-analyzer、gopls、Python/Pylance 和 TypeScript 配置了各自的项目入口。Cargo、`go.mod`、`pyproject.toml` 是 IDE 和语言工具的依赖事实来源，Bazel 从这些锁文件导入依赖并统一编排。

## 开发

要求 Bazel 9、Rust 1.85+、Go 1.26+、Python 3.12+ 和 uv。Pingora 的本机构建还需要 Clang；所选 rustls feature 不要求 OpenSSL 运行时。Web 使用 Bazel 下载的 Node/pnpm 工具链，不要求全局 Node 可用。

```bash
# 整个 monorepo
bazel build //...
bazel test //...

# 各语言原生反馈环（IDE 与局部开发）
(cd rust && cargo test --workspace)

# Go
(cd go && go test ./...)

# Python
(cd python && uv sync --extra dev && uv run pytest)

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

初版控制台包含平台总览、项目、API Key、模型/价格、费用估算和 Kubernetes `ModelDeployment` 管理。页面通过 Vite 代理连接本地组件；控制面未连接集群或 CRD 未安装时，部署页会进入只读保护态，其余管理功能仍可使用。

分别启动控制面和前端开发服务器：

```bash
cd go && go run ./control-plane/cmd/server

# 在仓库根目录的另一个终端
bazel run -- @pnpm//:pnpm --dir "$PWD/web" dev
```

打开 `http://127.0.0.1:5173`。生产静态文件位于 `bazel-bin/web/console/dist`。VS Code 补全依赖 `web/node_modules`，首次拉取代码后执行上面的 frozen-lockfile 安装命令即可。

启动开发 runtime 后，用 JSON 注入网关 Key 和 endpoint：

```bash
cd python && uv run xscope-runtime

cd rust
XSCOPE_API_KEYS_JSON='[{"id":"key-local","tenant_id":"tenant-local","project_id":"project-local","secret":"xscope-local-secret"}]' \
XSCOPE_UPSTREAMS_JSON='[{"id":"runtime-dev","address":"127.0.0.1:8090","weight":100}]' \
cargo run -p xscope-gateway

curl http://127.0.0.1:8080/v1/chat/completions \
  # Authorization credentials are supplied from a runtime secret.
  -H 'Content-Type: application/json' \
  -d '{"model":"xscope-demo","messages":[{"role":"user","content":"hello"}]}'
```

Pingora 在内存中只保留 Key 的 SHA-256 摘要，并在请求进入时做常量时间比较。已鉴权请求的 usage 通过 Serde 写入 append-only WAL；本地默认路径为 `/tmp/xscope-usage-v1.jsonl`。生产密钥应通过 Secret 或后续的签名配置快照下发。

Kubernetes Secret 的 `keys.json` 字段使用同一个 JSON 格式：

```bash
kubectl -n xscope-system create secret generic xscope-gateway-keys \
  --from-literal=keys.json='[{"id":"key-local","tenant_id":"tenant-local","project_id":"project-local","secret":"replace-this-secret"}]'
```

### Docker Desktop 一键部署

本地 overlay 会把管理面和数据面共同部署到当前 `kubectl` context，常驻 CPU request
约为 155m（不包含用户创建的模型工作负载）。控制台由 OAuth2 Proxy 保护，账号由
Keycloak 管理并持久化到 PostgreSQL；前端静态文件由控制面进程直接提供，减少一个
常驻 Pod。

```bash
./tools/deploy-local.sh

# 控制台 / 身份服务 / 推理入口
open http://localhost:30081
open http://xscope.localhost:30080
curl http://localhost:30082/healthz

# 查看运行状态与 Rust 网关结构化日志
kubectl get pods -n xscope-system -w
kubectl logs -n xscope-system deployment/gateway -f
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
cd go && go run ./operator/cmd/operator
```

## 垂直切片进度

1. 已完成：项目/API Key 管理、模型目录和报价、Ant Design 管理台、Pingora Key 认证/加权 endpoint/健康检查、OpenAI 非流式请求代理和 usage WAL。
2. 已完成：控制面创建/扩缩/删除 `ModelDeployment`，以及 Operator 创建 Deployment/Service 并回写 readiness。
3. 下一步：持久化组织/RBAC/项目/Key，并向网关发布签名配置快照。
4. 下一步：本地/Redis 配额预占、SSE 流式代理，以及把 WAL 可靠转发到 Kafka/Redpanda。
5. 下一步：计量消费者去重并聚合到账单账本；Operator 补 HPA/PDB/NetworkPolicy。

## 工程约定

- Bazel/Bzlmod 是统一 CI/发布构建入口；语言自身的 manifest 与 lockfile 是依赖事实来源，也保留原生 IDE、测试和本地反馈链路。
- 公共边界用 OpenAPI、JSON Schema，后续内部高频 RPC 再引入 Protobuf/gRPC。
- 金额使用最小货币单位整数，token/请求量使用整数；usage event 只追加、不原地更新。
- 所有资源都携带 `tenant_id`、`project_id`、`region`，所有写接口接受幂等键。
