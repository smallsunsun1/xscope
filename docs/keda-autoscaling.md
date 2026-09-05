# KEDA 接入：实现范围与运行约束

日期：2026-09-05。该文档记录第一批代码和集群验收，不代表完整 GPU/SLO 弹性已实现。

## 唯一副本写入链路

```text
ModelDeployment.spec.autoscaling
        ↓ XScope Operator
KEDA ScaledObject（与 ModelDeployment 同 namespace、同名）
        ↓ KEDA
HPA keda-hpa-<name> → ModelDeployment /scale
                            ↓ XScope Operator
                       Deployment.spec.replicas
```

用户和平台不再配置第二份 HPA。KEDA 不是替换 Kubernetes 水平扩缩容执行器，而是提供指标扩展及 ScaledObject 配置入口。平台采用上游 KEDA，不自行重写 scaler。

- ScaledObject、Deployment、Service、可选 PDB/InferencePool 都由同一个 ModelDeployment UID 持有；KEDA 的 HPA 则由 ScaledObject 持有。
- Operator 只能读取 HPA，以及删除经 UID 校验的旧版直接持有的 HPA；不能创建或修改 HPA。
- 迁移先删除旧 HPA，后续 reconcile 确认删除，再创建 ScaledObject。关闭自动扩缩容先移除 ScaledObject，等待生成的 HPA 回收，再恢复手动副本。
- 对同名或同目标的外来 ScaledObject/HPA 报冲突，不接管、不强行转移所有权。命名空间中其他合法 ScaledObject 的省略字段不会导致反序列化失败。
- KEDA 未安装时手动部署仍可 reconcile；启用 autoscaling 会报告明确依赖错误。
- ScaledObject 的 Ready 和 HPA 的 ScalingActive 汇入 ModelDeployment 的 AutoscalingActive。当前通过 reconcile 周期读取状态，不声称实时事件推送。

## 第一批支持的配置

以下是 CPU 测试基线，不是 GPU 生产策略：

```yaml
spec:
  replicas: 1
  resources:
    requests: {cpu: 20m, memory: 48Mi}
  autoscaling:
    minReplicas: 1
    maxReplicas: 2
    targetCpuUtilizationPercentage: 70
```

EPP Prometheus scaler 配置已生成，但正式启用前必须完成指标合同联调：

```yaml
spec:
  serving:
    endpointPickerService: example-serving
  autoscaling:
    minReplicas: 1
    maxReplicas: 4
    targetPendingRequests: 4
    targetRunningRequests: 10
```

`targetPendingRequests` 现在明确对应 EPP flow-control 队列 `llm_d_epp_flow_control_queue_size`，不是 runtime waiting；`targetRunningRequests` 对应 EPP `llm_d_epp_request_running`。二者是独立建议，不相加。没有 EPP 指标且未显式指定 CPU 时，兼容默认 CPU 70%。EPP 指标与显式 CPU 可同时配置。

迁移注意：旧 `targetPendingRequests` 曾指向未接通的自定义指标适配器。旧配置如果没有 `serving`，需要先补齐正确的入口合同或移除该阈值，不能无提示地沿用旧语义。本地迁移前没有存量 ModelDeployment；迁移测试另行构造了旧 HPA。

Prometheus 地址由 Operator 的 `XSCOPE_AUTOSCALING_PROMETHEUS_URL` 管理，默认 `http://prometheus.xscope-system.svc:9090`。不接受租户任意 PromQL 或 URL。查询以可信 `namespace/service` 标签隔离 serving 入口，校验每个发现的 instance 都有目标指标、抓取正常且样本最近 90 秒内更新；空数据用 `ignoreNullValues=false` 报错，不以 `or vector(0)` 伪装成零负载。服务发现本身丢失的实例还需要期望实例数合同才能检测，并未被这些检查覆盖。

现有本地 Prometheus 只发现 `inference-serving` 和 `inference-serving-canary`，新入口需要管理员扩充发现规则。标签修改必须部署 Prometheus 配置后才能生效。启用前还需要验证：EPP 版本/flow-control 配置确实输出这些指标；一个入口只服务目标 revision pool；多 EPP 计数是独立可加而非重复镜像。尚未验证的入口不要配置这两个阈值。

## 刻意保留的安全限制

- `minReplicas >= 1`，不启用 scale-to-zero。
- 扩容最多每 60 秒增加 1 个副本，30 秒稳定窗口，受 maxReplicas 限制。
- **自动缩容暂时禁用**（`scaleDown.selectPolicy: Disabled`）。目前还没有“停止接单 → SSE/生成排空 → 释放 KV/配额 → 删除 Pod”的闭环。PDB 不阻止 Deployment 自己缩容，不能代替 drain。
- 这里的上限只约束副本数，尚不等于资源池配额或 Kueue 准入。不得宣称能公平分配 GPU、借用/回收租户资源或维持 TTFT/TPOT SLO。

## 本地安装与验证

所有构建和测试通过 Bazel：

```sh
bazel test //rust/crates/kubernetes:kubernetes_test //tools:keda_config_test --test_output=errors
bazel run //tools:install_local_keda
./tools/bazel-linux.sh build //deploy/images:operator_load //deploy/images:cluster-agent_load --output_groups=tarball --config=release --jobs=4
# 将这两个 Bazel Linux 输出的 OCI tar 导入本地 Docker，再运行：
bazel run //tools:deploy_keda_operator
bazel run //tools:operator_cluster_smoke
```

安装器只允许 Docker Desktop，固定 KEDA 2.20.1 上游清单 SHA-256 和三个镜像摘要，保留 TLS、证书轮转、准入 webhook。每组件 1 副本、请求 25m CPU / 128Mi 内存，共请求 75m CPU / 384Mi 内存；这不是实际利用率或生产 HA 配置。安装器拒绝接管非 XScope 标记的既有资源，包括集群唯一 external-metrics APIService。生产使用管理员管理的 KEDA，并按 managed namespace 授权 CR `/scale`。

定向部署工具只更新模型 CRD、相关 RBAC、Operator、cluster-agent，不重启 gateway、control-plane、数据库或 Keycloak。KEDA 仍使用上游集群级 RBAC；附加 namespace Role 不是对上游全部权限的收窄。

集群 smoke 使用独立临时名称，覆盖外来 PDB 拒绝、旧 HPA 迁移、KEDA 所有权、真实 CPU 扩容、Pingora → EPP → Operator 管理的 Echo Pod、配置关闭和级联回收。只清理本次测试 UID 的资源，不重置业务数据。Echo + CPU 的验证不能替代真实 GPU 压测。

## 本地验收记录（2026-09-05）

KEDA 三组件及 external-metrics APIService 就绪，重复安装通过；Bazel 全仓 17 个测试目标通过。定向部署后的完整 smoke 通过，实际模型由 1 扩至 2 个 Ready 副本，再关闭策略、恢复手动扩缩并回收全部临时资源。首次测试发现旧 kubectl 默认读取 HPA v1，修正为显式 v2 后重跑成功。Docker Desktop 的 HPA 同步周期为 60 秒，因此测试允许多个采集/稳定周期，不修改集群全局控制器配置。实测轻载时 KEDA 共约 4m CPU、165Mi 内存，仅为当时快照。

## 后续顺序

1. 资源池/项目授权、容量核算、Kueue 准入与模型副本执行协调（desired / admitted / ready 分离）。
2. EPP/runtime 指标合同、revision/实例隔离、健康与缺数故障测试；CapacityProfile 定标。
3. 预热与 ServingReady、在线排空、长请求超时策略，完成安全缩容后才启用自动缩容。
4. TTFT/TPOT 反馈、前馈需求、SLO/Goodput 验收，以及多集群容量分配。UI 在这些后端合同验收后跟进。

查看当前集群状态（显式读取 HPA v2，兼容本机旧 kubectl）：

```sh
kubectl --context docker-desktop -n keda get pods
kubectl --context docker-desktop -n xscope-system get modeldeployments,scaledobjects,horizontalpodautoscalers.v2.autoscaling
```

没有启用 autoscaling 的模型时，列表中没有 ScaledObject/HPA 是正常的；集群测试结束后会回收自己的临时模型。
