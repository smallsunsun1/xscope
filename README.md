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
| `web` | npm、TypeScript | 管理控制台 |
| `api` / `deploy` | OpenAPI、JSON Schema、YAML | 对外契约和 Kubernetes 清单 |

推荐直接打开 `xscope.code-workspace`。它已经为 rust-analyzer、gopls、Python/Pylance 和 TypeScript 配置了各自的项目入口。Cargo、`go.mod`、`pyproject.toml` 是 IDE 和语言工具的依赖事实来源，Bazel 从这些锁文件导入依赖并统一编排。

## 开发

要求 Rust 1.85+、Go 1.26+、Python 3.12+ 和 uv。Pingora 的本机构建还需要 Clang；所选 rustls feature 不要求 OpenSSL 运行时。

```bash
# 整个 monorepo
bazel build //...
bazel test //...

# 各语言原生反馈环（IDE 与局部开发）
cd rust && cargo test --workspace

# Go
cd go && go test ./...

# Python
cd python && uv sync --extra dev && uv run pytest

# Web
cd web && npm install && npm run check
```

Bazel 依赖映射：Go 由 Gazelle 读取 `go/go.mod`，Rust crate_universe 读取 `rust/Cargo.toml` 和 `rust/Cargo.lock`，Python pip hub 读取 uv 生成的 `python/requirements.lock`。依赖变化后分别运行 `go mod tidy`、`cargo update` 或 `uv lock`，再刷新对应 lockfile/BUILD 文件。

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

Operator 必须运行在集群内或具备有效 kubeconfig：

```bash
cd go && go run ./operator/cmd/operator
```

## 垂直切片进度

1. 已完成：项目创建、摘要存储 API Key、Pingora Key 认证/加权 endpoint/健康检查、OpenAI 非流式请求代理、usage WAL，以及基础 K8s reconcile。
2. 下一步：组织与 RBAC，并向网关发布签名配置快照。
3. 下一步：本地/Redis 配额预占、SSE 流式代理，以及把 WAL 可靠转发到 Kafka/Redpanda。
4. 下一步：计量消费者去重并聚合到账单账本。
5. 下一步：Operator 根据 `ModelDeployment` 创建 Deployment/Service/HPA 并回写 readiness。

## 工程约定

- Bazel/Bzlmod 是统一 CI/发布构建入口；语言自身的 manifest 与 lockfile 是依赖事实来源，也保留原生 IDE、测试和本地反馈链路。
- 公共边界用 OpenAPI、JSON Schema，后续内部高频 RPC 再引入 Protobuf/gRPC。
- 金额使用最小货币单位整数，token/请求量使用整数；usage event 只追加、不原地更新。
- 所有资源都携带 `tenant_id`、`project_id`、`region`，所有写接口接受幂等键。
