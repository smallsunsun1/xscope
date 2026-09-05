# 项目级流量路由（阶段 3）

本地管理入口：<http://localhost:30081/#/routing>。只有对应租户的 owner 可以修改；成员可以查看。
所有应用构建、测试与镜像打包仍经过 Bazel。业务数据使用 SeaORM 和 PostgreSQL；本次只新增
`xscope.route_policies` 表，不重置用户、账本、WAL、Redis 或监控存储。

## 已实现的边界

- 一个项目、一个公开模型对应一份 RoutePolicy。当前公开模型仍只有 `xscope-demo`，不是任意多模型路由。
- Stable / Canary 对应预注册模型池，不接受用户提交的地址。网关只选择 serving Service；
  Envoy + llm-d EPP 仍负责 InferencePool 内的 Pod 选择。
- 请求头规则按列表顺序首次精确匹配；无匹配时使用 0–100% 的 Canary 权重。
  请求头名仅允许小写 `x-route-*`，HTTP 名称匹配不区分大小写，值区分大小写。
  重复同名请求头不匹配规则；最多 16 条规则，禁止重复 name/value 条件。
- 权重使用服务端随机数，与客户端 `x-request-id` 无关；它是统计分流，不保证每 10 个请求恰好 1 个灰度。
- API key、租户/项目、模型权限、RPM/TPM、预算门禁继续生效。请求头选择版本，不授予权限。
- Keys 和 RoutePolicy 通过同一原子快照更新，请求保留认证时的策略。默认每 5 秒刷新；
  无效快照保留上一次有效配置。控制面故障期间继续使用最后的有效快照，尚无多副本 ACK/强一致吊销协议。
- 配置了未知池返回 503；选中的 serving 连接失败返回 502/503；EPP 故障返回上游错误。
  不会自动退回 Stable 并重放 POST。正在执行的 SSE 不因策略更新切池，取消仍向下游传播。

Pingora 在请求体过滤之前建立 upstream，因此当前按网关唯一公开模型选池。请求体中的 model
仍会再次校验，提示词在校验/配额预占之前不会被释放。不能将此实现宣称为多模型请求体路由。

## 如何操作

1. 打开“流量路由”，找到自己的项目，点击“配置策略”。
2. Stable 选择 `demo-pool`，Canary 选择 `demo-pool-canary`，比例保持 **0%**。
3. 添加 `x-route-cohort` 精确匹配 `qa` → Canary，确认发布。
4. 等待约 5 秒，用该项目的 API key 调用 gateway：

```bash
curl -i -N http://localhost:30082/v1/chat/completions \
  -H "Authorization: Bearer $XSCOPE_TEST_API_KEY" \
  -H 'Content-Type: application/json' \
  -H 'x-route-cohort: qa' \
  -d '{"model":"xscope-demo","stream":true,"messages":[{"role":"user","content":"hello canary"}]}'
```

响应头 `x-xscope-pool` 应为 `demo-pool-canary`，`x-xscope-model-revision` 为 `canary-echo-v1`。
不带 cohort 头时进入 Stable。验证后可以逐步增大比例。完整回退需把比例设为 0%，**并移除指向
Canary 的请求头规则**。发布是异步快照分发，界面显示的是“已保存版本”，不是所有网关已 ACK。

当前两个池都运行 **echo Runtime，不是 GPU/vLLM**。Canary 增加一个 Envoy/EPP Pod（50m CPU request）
和一个 echo Pod（20m），合计 70m CPU、208Mi 内存 request，仅 local overlay 启用。
两个池的 Deployment / Service / InferencePool 选择器互不重叠。

## API

控制台登录会话调用 `/api/admin/v1`，示例相对路径如下：

- `GET /route-pools`：部署注册的池和模型版本，不包含内部地址。
- `GET /route-policies`：当前用户可访问租户的策略。
- `PUT /projects/{project_id}/models/{model}/route-policy`：仅 owner；请求如下。

```json
{
  "expected_revision": 0,
  "spec": {
    "stable_pool": "demo-pool",
    "canary_pool": "demo-pool-canary",
    "canary_percent": 0,
    "headers": [{"name": "x-route-cohort", "value": "qa", "target": "canary"}]
  }
}
```

首次创建用 0，更新必须带读取到的 revision。PostgreSQL 条件 UPDATE 原子比较并递增版本，
并发写只有一个成功，另一个返回 409；网络结果不确定时重新 GET 核对，不盲目覆盖。
回退也是 PUT，新版本递增，没有删除后重新从 1 计数的 ABA 问题。尚无完整策略历史/审批系统。

## 证据和测试

- 响应头：`x-xscope-pool`、`x-xscope-model-revision`、`x-xscope-route-revision`、`x-trace-id`。
- 用量记录持久化实际 pool / model_revision；定价版本仍独立于模型版本。
- Jaeger gateway span 增加 `xscope.route.pool`、`xscope.route.revision`、`xscope.model.revision`。
- Prometheus `xscope_route_selections_total{pool,reason}` 只使用注册池和固定原因，
  表示请求体准入前的选池次数，不等同于成功推理/可计费请求数。Canary Pod 也纳入抓取。

```bash
bazel test //... --jobs=3
bash tools/bazel-linux.sh images
bazel run //tools:deploy_routing
bazel run //tools:routing_cluster_smoke
bazel run //tools:inference_cluster_smoke
bazel run //tools:telemetry_cluster_smoke
```

离线 TCP 测试覆盖动态快照、无效快照保留、租户隔离、头路由、0/100% 分流、缺失/故障池不回退、
SSE 与取消。集群 smoke 使用管理员 kubectl 临时隧道（不是 OIDC 登录测试），创建隔离测试项目，
验证权限、并发 CAS、真实 EPP 选择的 Pod 和用量版本；最后吊销测试 key，保留项目/账本证据，
不修改其他项目策略。所有临时隧道自动关闭，用户访问仍走 NodePort。

下一阶段：Operator 为 ModelDeployment 管理动态 InferencePool、HPA、PDB，并明确与 llm-d
安装资源的所有权。当前池仍由安装清单管理；新建 ModelDeployment 不会自动接入这两个池。
