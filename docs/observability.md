# 本地 tracing 与 metrics

Rust 服务通过 `rust/crates/telemetry` 统一初始化 tracing、OpenTelemetry
和 Prometheus。Python Runtime 使用 OpenTelemetry SDK 与 Prometheus client。
部署配置位于 `deploy/k8s/observability`，没有增加公开的管理端口。

## 查看

分别在两个终端启动（保持终端运行）：

```sh
kubectl --context docker-desktop -n xscope-system port-forward svc/jaeger 16686:16686
kubectl --context docker-desktop -n xscope-system port-forward svc/prometheus 9090:9090
```

- Jaeger：<http://localhost:16686>，选择 `xscope-gateway` 搜索推理请求。
- Prometheus：<http://localhost:9090>，在 Targets 中检查采集状态。
- 原控制台仍为 <http://localhost:30081/>；推理入口仍为 <http://localhost:30082>。

本地端口转发只用于开发；这两个查询界面没有另外接入 Keycloak，勿直接公开。

## 已实现与验证

- 请求使用 W3C `traceparent` / `tracestate` 传播，响应返回 `x-trace-id`。
  不传播 baggage，不采集请求正文、API Key 或 Authorization。
- 已在同一 trace 中验证 Gateway → Envoy → Runtime，以及异步 usage 上报到
  Control Plane。Control Plane 调用 cluster-agent 时也注入上下文。
- SSE span 覆盖响应生命周期，记录成功、失败和取消；first-byte 指标是首个
  响应 body 字节延迟，不等同于模型的首 token 延迟（TTFT）。
- Prometheus 按 Pod 采集 Gateway、Control Plane、cluster-agent、Operator、
  Runtime、Envoy 和 EPP。多副本不通过 Service 负载均衡采集。
- Rust HTTP 指标包含请求数、耗时、in-flight 和首字节延迟；另有 token、
  reconcile、WAL 写入和 usage 投递结果指标。路由使用模板名，避免将对象名、
  用户、Key 或 request ID 放入指标标签。

示例 PromQL：

```promql
sum by (component) (up{job="xscope"})
sum by (outcome) (rate(xscope_http_requests_total{component="gateway"}[5m]))
histogram_quantile(0.95, sum by (le) (rate(xscope_http_duration_seconds_bucket{component="gateway"}[5m])))
```

通过 Bazel 执行验证：

```sh
bazel test //tools:observability_config_test //tools:streaming_e2e_test
bazel run //tools:telemetry_cluster_smoke
bazel run //tools:kubernetes_cluster_smoke
bazel run //tools:inference_cluster_smoke
```

集群 smoke 限定本地 Docker Desktop。推理 smoke 会产生少量开发用量和账本记录，
不会重置数据；Kubernetes smoke 创建临时 echo 工作负载，测试扩缩容后删除并验证回收。
这不是 GPU 模型性能测试。

## 资源与尚未完成的部分

Prometheus 和 Jaeger 各请求 20m CPU，总计新增 40m CPU request；这不是实测
CPU 使用量。Prometheus 使用 emptyDir，保留上限 24 小时 / 256 MB；Jaeger 使用
内存存储，最多 1,000 条 trace，重启不保留。本地默认全量采样，生产环境需要
调整采样率、持久化、访问控制和保留策略。

EPP 已开启 tracing，metrics 已验证可采集，但尚未验证 EPP span 与上游同一
trace 的关联，因此不能将当前状态称为每一跳完整追踪。数据库查询 span、
WAL 重放后的跨进程 trace 连续性也尚未实现。

已有 target down、推理错误率和 usage 持久化/投递失败的 Prometheus 告警规则，
尚未配置 Alertmanager 通知接收方。正式 SLO、不可变审计、告警闭环仍属于
后续阶段，不因接入 tracing/metrics 而视为完成。
