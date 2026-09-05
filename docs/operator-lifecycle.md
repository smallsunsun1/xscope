# Operator：扩缩容、故障保护与推理池

本阶段保留 Rust/kube-rs 的进程边界：业务控制面没有 Kubernetes 凭证，通过内部鉴权的 cluster-agent 写入 `ModelDeployment`；独立 Operator 将声明收敛为集群资源。业务数据库仍使用 SeaORM。本页不涉及业务数据迁移。

2026-09-05 更新：扩缩容改为 [KEDA ScaledObject 单入口](keda-autoscaling.md)。下文原始阶段验收中的直接 HPA 和缩容稳定窗口属于迁移前记录；当前自动缩容禁用，直至排空能力完成。

## 资源所有权

| 资源 | 所有者及行为 |
| --- | --- |
| `ModelDeployment` | cluster-agent 接受创建、版本化更新、手动扩缩、删除；HPA 通过其 `/scale` 子资源更新副本数 |
| Runtime Deployment、内部 Service | Operator 创建、修复漂移，通过 ownerReference 随模型删除 |
| ScaledObject、PDB、InferencePool | 配置启用时由 Operator 创建；关闭时仅删除本模型 UID 所拥有的对象 |
| HPA | KEDA 创建并持有；Operator 只读，除迁移时删除旧版直接持有的 HPA |
| Envoy / llm-d EPP Deployment、Service、配置、证书、ServiceAccount、RBAC | 安装层所有，不由 Operator 创建、接管或删除；每个 EPP 显式绑定一个 Pool |
| 上游 InferencePool/KEDA CRD、KEDA、metrics-server | 集群基础设施所有，独立安装；不属于模型资源 |

所有写入前先检查同名资源的归属，拒绝接管外部对象；更新和删除使用 UID/resourceVersion 前置条件。Runtime Pod 禁止自动挂载 ServiceAccount token，池选择器包含模型对象 UID，防止模型名称复用后混入旧 Pod。已有 Deployment 的不可变 selector 保留。

## 声明示例

下面是 `ModelDeployment.spec` 示例。模型创建入口仍为 `POST /api/admin/v1/model-deployments`，请求携带完整 CR 的 apiVersion、kind、metadata、spec，并要求已登录的平台 owner 权限。

```yaml
model:
  id: xscope-demo
  revision: v1
  uri: echo://development
  checksum: sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
runtime:
  image: xscope/runtime:dev
  protocol: openai
  port: 8090
replicas: 1
resources:
  requests: {cpu: 20m, memory: 48Mi}
  limits: {cpu: 200m, memory: 192Mi}
autoscaling:
  minReplicas: 1
  maxReplicas: 3
  targetCpuUtilizationPercentage: 70
disruptionBudget:
  maxUnavailable: 1
serving:
  endpointPickerService: example-serving
  endpointPickerPort: 9002
```

启用 `serving` 前，须由安装层准备同 namespace 的 EPP Service，暴露对应端口，并设置 `platform.xscope.io/inference-pool: <ModelDeployment 名称>` annotation；EPP 进程的 `--pool-name` 必须一致。Operator 创建与模型同名的 InferencePool，并使用 `FailClose`。现有 `deploy/k8s/serving` 是安装层模板，不能把已绑定 `demo-pool` 的 EPP 直接复用到新池。

**创建池不等于自动对外发布。** 新池仍需要对应的 Envoy/EPP serving entry，并加入 Gateway 的已注册服务目录；RoutePolicy 只能选择已注册池。`status.endpoint` 是 Runtime 内部 Service，不应把它注册为 Gateway 入口。动态 serving entry 编排/分发属于后续控制同步工作，本阶段不会把未准备好的模型直接暴露给客户。

## 修改、扩缩与状态

完整更新入口：`PUT /api/admin/v1/model-deployments/{namespace}/{name}`。

```json
{"resourceVersion":"从列表响应读取的当前值","spec":{"...":"完整的新 spec"}}
```

这是 spec 替换，不是 merge patch；从列表读取当前 spec，修改后连同 resourceVersion 提交。过期版本返回 409，应重新读取，避免覆盖 HPA 或其他操作者的更新。metadata、UID、ownerReference、status 不接受此接口覆盖。

- HPA 以 `ModelDeployment` 为 scale target，Operator 再同步 Deployment，避免两个控制器争抢 Deployment 副本数。
- 启用 HPA 时，手动 `/scale` 返回 400。移除 `autoscaling` 后才可手动缩到 0；HPA 不支持本阶段的 scale-to-zero。
- `minReplicas >= 1`，必须提供 `maxReplicas`，当前 replicas 应处于区间内。CPU 指标要求配置 CPU requests；未选择 pending 指标时默认 CPU 70%。
- 可设置 `targetPendingRequests` 生成 `xscope_pending_requests` Pods 平均值指标，亦可与 CPU 指标并用；**自定义指标适配器和 Runtime 队列指标尚未安装/联调，不应把这个选项视为可用的 GPU 队列扩容能力。**
- PDB 保护自愿驱逐，不保证节点故障下可用性。单副本 `maxUnavailable: 1` 允许短暂不可用；设为 0 会阻止自愿驱逐，需要自行权衡。
- 删除 `disruptionBudget` / `serving` 会清理对应的自有 PDB / Pool，但不会删除安装层 EPP。删除整个模型由 Kubernetes GC 清理自有子资源。
- `ResourcesReady` 表示资源收敛；`Ready` 表示 Runtime 就绪，不代表外部 EPP 或公共入口就绪；`AutoscalingActive` 对应 HPA 的实际指标状态。`status.replicas` 为实际副本数，`readyReplicas` 单独表达就绪数。
- 不再接受旧 `rollout.canaryWeight > 0` 这个此前未执行的选项；灰度流量使用 RoutePolicy 的 stable/canary 池。

## 本地部署和验收（Bazel-only）

```bash
# Linux 镜像由 Bazel 编译打包并载入 Docker Desktop
./tools/bazel-linux.sh images
# 仅更新模型 CRD、Operator RBAC 及三个相关 Rust 服务；不重置数据
bazel run //tools:deploy_operator
# Docker Desktop 缺少 metrics API 时安装；拒绝覆盖别人的同名安装
bazel run //tools:install_local_metrics

bazel test //...
bazel run //tools:operator_cluster_smoke
bazel run //tools:kubernetes_cluster_smoke
bazel run //tools:inference_cluster_smoke

kubectl get modeldeployments,hpa,pdb,inferencepools -n xscope-system
kubectl top pods -n xscope-system
```

本地 metrics-server 使用上游 v0.8.1 固定摘要，requests 为 30m CPU / 96Mi 内存。Docker Desktop kubelet 使用自签名证书，因此安装工具带有 **仅限本地的 `--kubelet-insecure-tls` 例外**，只允许 docker-desktop context；生产必须配置可信 CA，不得照搬这个 TLS 配置。Prometheus 与 metrics-server 用途不同，HPA CPU 指标来自后者，不依赖 Grafana。

专用 smoke 会创建唯一命名的模型、独立 EPP/Envoy、临时 Gateway 和 Secret，验证归属冲突不接管、PDB 漂移修复、真实 SSE 选中模型 Pod、真实 CPU HPA 扩容、版本冲突、关闭配置、GC；退出时清理这些临时资源。它不注册公共客户容量，也不写入现有业务账本。临时 Runtime 是 Echo，不是 GPU 模型。现有模型池与数据保持不变。

协议参考：[Kubernetes HPA](https://kubernetes.io/docs/concepts/workloads/autoscaling/horizontal-pod-autoscale/)、[CRD scale subresource](https://kubernetes.io/docs/tasks/extend-kubernetes/custom-resources/custom-resource-definitions/)、[metrics-server](https://github.com/kubernetes-sigs/metrics-server)。
