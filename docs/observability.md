# 本地 tracing 与 metrics

Rust 服务通过 `rust/crates/telemetry` 统一初始化 tracing、OpenTelemetry
和 Prometheus。Python Runtime 使用 OpenTelemetry SDK 与 Prometheus client。
基础部署配置位于 `deploy/k8s/observability`，默认只提供私有服务；local overlay
额外将需要登录的 Grafana 暴露为 NodePort 30083。

## 查看

本地 Grafana 直接访问 <http://localhost:30083>，不需要 `port-forward`。
只要 Docker Desktop Kubernetes 和 Grafana 正常运行，该入口就可访问，不依赖终端。
在 Grafana 中既能查看 Prometheus 指标，也能通过 Explore → Jaeger 查看 trace。

如果需要访问原生 Jaeger/Prometheus 页面，再按需在独立终端启动（保持终端运行）：

```sh
kubectl --context docker-desktop -n xscope-system port-forward svc/jaeger 16686:16686
kubectl --context docker-desktop -n xscope-system port-forward svc/prometheus 9090:9090
```

- Jaeger：<http://localhost:16686>，选择 `xscope-gateway` 搜索推理请求。
- Prometheus：<http://localhost:9090>，在 Targets 中检查采集状态。
- Grafana：<http://localhost:30083>，登录后打开 Dashboards → XScope → XScope Overview。
  已自动配置 Prometheus 与 Jaeger 数据源；Explore 中可以选择 Jaeger 查询 trace。
  概览顶部展示当前请求速率、P95 延迟、并发和在线采集数，下方分为推理表现与
  投递/采集状态；Pod 状态时间线保留历史、按 7 行分页。无样本显示 `—`，不等同于 0。
- 请求追踪看板：<http://localhost:30083/d/xscope-traces/xscope-traces>，也可从概览顶部
  “请求追踪”链接进入。按服务、时间、最小耗时、操作名和标签筛选，点击表格里的
  Trace ID 打开 Explore 中的完整调用链。每 30 秒刷新，最多返回 100 条；表格支持
  排序和列筛选。默认服务为 `xscope-gateway`；服务选项在看板 JSON 中管理。
  表格固定 Trace ID 首列并分页，蓝色耗时条按本次结果范围比较，不是 SLO 阈值。
- 原控制台仍为 <http://localhost:30081/>；推理入口仍为 <http://localhost:30082>。

本地端口转发只用于开发。Prometheus/Jaeger 查询界面没有另外接入 Keycloak，勿直接公开。
NodePort 并不保证仅绑定 localhost，是否能从局域网访问取决于 Docker Desktop 和宿主机
网络/防火墙配置。因此 30083 仅用于可信本地开发环境，不应用于共享或公网集群。
基础配置仍为 ClusterIP；只有 `deploy/k8s/overlays/local` 启用 NodePort 和对应入口策略。
更新本地监控时应保留该 overlay；直接应用 observability 基础目录会恢复私有入口。
Grafana 禁止匿名访问和注册，使用独立随机管理员密码；本次尚未接入 Keycloak SSO
或控制台入口。用户名为 `admin`，在你自己的终端读取初始密码：

```sh
kubectl --context docker-desktop -n xscope-system get secret xscope-grafana \
  -o jsonpath='{.data.admin-password}' | base64 --decode
```

`bazel run //tools:observability_admin` 仅在 Secret 不存在时生成凭据，重复部署不会
重置密码或加密密钥。在 Grafana 修改密码后，Secret 中的初始密码不会自动更新。
内置看板由仓库中的 JSON 管理；要在 UI 自定义，请 Save as copy，新看板存入 PVC
上的 SQLite 数据库，不会在配置刷新时被覆盖。

“XScope 请求追踪”的列表来自 Jaeger Search，不是固定 Trace ID，也不是模拟数据。
它仅展示当前时间范围内已经采集并尚未过期的 trace，不应当作全部请求数量；请求
速率/错误率请看 Prometheus。最小耗时是服务匹配 span 的 Jaeger 查询条件，不一定
等于异步用量上报也包含在内的完整 trace 总时长。标签使用 Jaeger logfmt 语法，
例如 `http.route="/v1/chat/completions"`；操作名需要填写准确的 span 名，如 `http.request`。

更新仓库中的看板后，可仅部署看板配置，不影响 NodePort 或其他组件：

```sh
bazel run //tools:deploy_grafana_dashboards
bazel test //tools:trace_dashboard_config_test
bazel run //tools:trace_dashboard_smoke
```

部署会因 ConfigMap 名称变化更新 Grafana Pod，短暂停顿后原 NodePort 恢复；不改动
用户看板、密码或 PVC。最后一条命令通过 NodePort 只读验证真实搜索、Trace ID
详情跳转所用的查询，以及耗时筛选；需要最近一小时已有推理 trace。

## 已实现与验证

- 请求使用 W3C `traceparent` / `tracestate` 传播，响应返回 `x-trace-id`。
  不传播 baggage，不采集请求正文、API Key 或 Authorization。
- 已在同一 trace 中验证 Gateway → Envoy → EPP / Runtime，以及异步 usage 上报到
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

## 持久化、迁移和资源

| 服务 | PVC | 保存内容与保留策略 |
| --- | --- | --- |
| Prometheus | `prometheus-data`，2 GiB | `/prometheus` TSDB/WAL；7 天或 1 GB，先到为准，预留 head/WAL 空间 |
| Jaeger | `jaeger-data`，2 GiB | `/var/lib/jaeger` Badger；trace TTL 72 小时，开启同步写盘 |
| Grafana | `grafana-data`，1 GiB | `/var/lib/grafana` SQLite：用户、个人看板与设置 |

三者均为单副本、Recreate 更新策略，避免并发进程争用本地数据库。PVC 是独立资源，
没有 Deployment ownerReference，普通 Pod 重建或 Deployment 更新不删除 PVC。
Grafana 的凭据和数据源加密密钥另外保存在 Kubernetes Secret `xscope-grafana`，备份
Grafana 时必须一起保留这个 Secret。仓库自动加载的数据源/看板另外由 ConfigMap 管理。

每个服务请求 20m CPU，三个共 60m；这是调度 request，不是实测使用量。
Grafana 内存 request 为 256 MiB、limit 为 768 MiB，`GOMEMLIMIT=512MiB` 为 Go GC
提供软内存目标。此前 384 MiB 的容器上限在浏览看板时发生过 OOMKilled，已调高；
NodePort 消除了终端依赖，但不能消除 Pod 重启期间的短暂不可用。
Grafana 不安装额外插件，也不额外部署 Elasticsearch 等数据库。本地 Jaeger 使用
内嵌 Badger，仅适合单节点开发，不能直接扩成共享存储多副本。

首次将旧部署的 emptyDir 切换到 PVC 前：

```sh
kubectl --context docker-desktop apply -f deploy/k8s/observability/storage.yaml
bazel run //tools:observability_migrate -- --backup-dir /absolute/path/to/new-private-backup
```

迁移工具拒绝覆盖非空 PVC：先导出当前可查询的 Jaeger trace 为 JSON 备份，再短暂
暂停 Prometheus 并复制 TSDB/WAL 至 PVC，然后恢复旧进程。复制完成到切换部署之间
新采集的样本不在快照内，切换时会短暂缺失监控。旧 Jaeger 内存 trace 不自动导入
Badger，仍可从私有备份文件恢复查看。完成 PVC 迁移后不要再次运行此工具。

在本地实际重建三个监控 Pod 并验证持久化（会有短暂监控停顿）：

```sh
bazel run //tools:observability_persistence_smoke -- --restart
```

它验证重建前的历史指标、同一 trace 和临时创建的 Grafana 看板仍能查询，随后删除
它自己的临时看板；不删除 PVC，不重置业务数据。

**PVC 不等于备份或高可用。** Docker Desktop 当前默认 `hostpath` 的回收策略是
`Delete`，重置 Docker Desktop、删除 PVC 或丢失宿主机磁盘仍会丢数据。不要对该
目录执行 `kubectl delete -k` 来更新部署，应使用 `apply -k`。需要跨机器容灾时，
还应增加定期离线备份/快照与远端存储。Badger TTL 不等同于磁盘硬限额，应监控
实际容量；生产集群还需要采样策略、存储容量告警、持久化后端与恢复演练。

## 尚未完成的部分

本地 smoke 已验证 EPP span 与上游关联。数据库查询 span、WAL 重放后的跨进程
trace 连续性尚未实现；开发 echo 链路的验证不能代替 GPU/vLLM 的生产验证。

已有 target down、推理错误率和 usage 持久化/投递失败的 Prometheus 告警规则，
尚未配置 Alertmanager 通知接收方。正式 SLO、不可变审计、告警闭环仍属于
后续阶段，不因接入 tracing/metrics 而视为完成。
