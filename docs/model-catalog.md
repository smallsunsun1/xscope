# 模型目录与多模型发布

2026-09-12 后续增量已补上 [受管入口的自动注册、ACK 和手动排空缩容](managed-traffic.md)。本页下文“仍需完成”的描述记录上一增量边界；受管流程不覆盖旧入口或 KEDA 自动缩容。

本页描述代码契约，不代表当前 Kubernetes 已部署此版本。所有例子均为合成值；实际部署配置和凭据使用 Secret，不写入仓库或命令参数。

## 已落地的调用路径

控制面使用 SeaORM 管理 PostgreSQL `xscope.model_catalog`、`model_prices`、`serving_endpoints`。API Key 的模型权限、报价和新预占查询模型目录，不再绑定一个固定模型。模型禁用后不能新建预占，已经存在的预占仍按冻结条款恢复、结算；用量 v1 重试使用对应历史价格。价格版本绑定完整模型定义，修改定义需新版本。价格历史受数据库 append-only 触发器保护。

网关先认证，再在 15 秒内读取最多 1 MiB 的请求，按请求体 `model` 选择该项目的 RoutePolicy 或模型默认 Pool。权限、模型和 Pool 三者必须一致。价格、上下文上限和目标 revision 在本次请求内固定，不受随后快照更新影响。`XSCOPE_MODEL_CONTEXT_TOKENS` 仍是网关的额外上下文上限，取它与目录上限的较小值。

目录、密钥和策略作为一份快照在网关原子替换；无效快照保留最后有效版本。兼容不含目录的旧快照；旧配置同一模型有多个 Pool 时仍优先使用明确配置的默认入口，不能随机选中 canary。目录中未配置默认 Pool 的模型需要项目 RoutePolicy，否则返回 503。`GET /v1/models` 需要 Bearer API Key，列出该 Key 可访问且启用的模型；列出不表示 Runtime 当前健康。

注册的地址是 Envoy/llm-d 的服务入口，不是模型 Pod。运行时解析 Service DNS，不在网关实现 Pod 负载均衡。未知、跨模型或不可达 Pool 失败关闭，不跨 Pool 重试推理 POST。用量继续使用有界易失 HTTP 队列，没有 WAL、SQLite 或对象归档。

目录模式下 `/readyz` 检查网关自身的密钥和计费投递能力，不因默认模型故障摘掉整个网关，其他模型仍可用。单模型健康应通过对应 Runtime/服务监控判断；旧的无目录快照模式仍保留默认入口健康检查。

Pingora 0.8.1 的 H1/H2 缓存上限通过一个小型 Bazel 依赖补丁与 1 MiB 请求上限对齐，以支持选路前读取请求体。该缓存用于转发原请求，不授权推理重试；Bazel TCP 回归覆盖超过原 64 KiB 上限的请求、超限拒绝和流式取消。

## 管理接口

目录和服务入口写入需要平台管理员，不是普通租户成员。沿用控制台会话和操作审计，不接受 Gateway 内部 token 作为控制台登录。

| 接口 | 用途 |
| --- | --- |
| `GET /admin/v1/model-catalog` | 当前目录与 revision，包括禁用模型 |
| `PUT /admin/v1/model-catalog/{id}` | 创建或 CAS 更新模型；`expected_revision=0` 创建 |
| `PUT /admin/v1/serving-endpoints/{id}` | 注册或 CAS 更新入口；`expected_generation=0` 创建 |
| `GET /admin/v1/serving-endpoints` | 平台管理员读取入口配置和当前 generation；响应包含私有地址，不应复制到公开文档 |
| `GET /admin/v1/route-pools` | 合并启动配置和数据库中的 Pool 元数据，不返回地址 |
| `GET /admin/v1/quote?model=synthetic-model&input_tokens=100&output_tokens=100` | 使用模型当前价格报价 |

模型写入示例：

```json
{
  "expected_revision": 0,
  "enabled": true,
  "default_pool": "synthetic-stable",
  "model": {
    "id": "synthetic-model",
    "display_name": "Synthetic model",
    "max_context_tokens": 32768,
    "price_version": "synthetic-price-v1",
    "input_per_million_tokens": {"currency": "CNY", "amount": 1000},
    "output_per_million_tokens": {"currency": "CNY", "amount": 2000}
  }
}
```

入口请求由 `expected_generation` 与 `endpoint` 组成；后者包含 `id/model/revision/address/tls/server_name`。实际 `address` 应来自部署 Secret，不能从客户端推理请求透传。Pool 的 `model/revision` 不能重绑定，发布新 revision 要使用新 Pool ID。更新成功返回新 generation；并发旧版本写入返回 409。

首次启动只幂等创建默认开发模型，不覆盖已经编辑或禁用的记录。新增 migration 000015/000016 都是增量迁移；不重置 Keycloak、业务账本或历史对象证据。部署后不要回退到只支持单模型定价的控制面旧版本。

## 发布动作与历史

项目 owner 可以写入和操作项目路由；历史读取仍受项目租户权限约束。

```text
PUT  /admin/v1/projects/{project}/models/{model}/route-policy
POST /admin/v1/projects/{project}/models/{model}/route-policy/actions
GET  /admin/v1/projects/{project}/models/{model}/route-policy/history?limit=20
```

动作请求示例：

```json
{"action":"pause","expected_revision":3}
```

- `pause`：canary 权重归零，并移除所有指向 canary 的 header 规则，避免请求头绕过暂停。
- `promote`：将 canary 设为 stable，清空该轮灰度规则；没有 canary 时拒绝。
- `rollback`：额外传 `target_revision`，恢复较早的已记录策略，但生成新的递增 revision。

每次 PUT/动作的当前策略和 `route_revisions` 历史在同一事务提交。并发写入只有一个成功；历史写入失败不会留下新路由。旧版迁移只记录当时的当前策略为 `legacy_snapshot`，不编造更早的历史。历史不可改删，按 revision 倒序分页，最大 100 条，使用返回的 `next_before` 查询后续页。

这些动作改变的是后续请求的选路策略。暂停不是终止已在运行的 SSE，也不表示所有 Gateway 已 ACK，更不能立即删除旧 Pool。回滚前仍需确保目标 Pool 可用；目前没有自动 SLO 门禁或完整的 Gateway drain/ACK 发布控制器。

## Runtime 启动与就绪

`ModelDeployment.spec.runtime.health` 支持引擎自己的健康路径：

```yaml
health:
  path: /health
  startupTimeoutSeconds: 1800
```

省略时沿用 `/readyz`。Operator 同时生成 startupProbe 和 readinessProbe，启动等待范围 10–7200 秒、5 秒步进，不再用固定 `/readyz` 限制所有 Runtime。该 HTTP 路径必须由引擎在模型加载完成后正确返回就绪；探针不是模型下载、校验或 warmup 实现。真实 GPU 引擎仍需单独验收。

## 验证与尚未完成的部分

```bash
bazel test //... --jobs=3 --test_output=errors
bazel run //tools:billing_protocol_smoke --jobs=3
```

前者包含两个独立 Runtime 进程的多模型、流式取消和热更新测试；后者使用临时 PostgreSQL、两个控制面及两个 Gateway 验证冻结价格、权限、CAS、重复结算、发布历史与事务故障。均不是 GPU 性能测试，也不修改在线 K8s。

仍需依次完成：部署到目录的自动注册与就绪观测、Gateway ACK/旧 Pool 排空及安全缩容、资源准入；然后推进计费吞吐和长期数据库恢复、多集群实接、告警通知与支付/税务/管理入口。不能将本次目录和发布 API 视为原六项规划全部完成。
