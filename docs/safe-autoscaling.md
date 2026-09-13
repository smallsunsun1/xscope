# KEDA 自动缩容与 Gateway 终止证明

本次增加受管 v2 入口的自动扩缩逻辑，不再只提供手动排空。旧入口仍保持原来的安全限制，不自动切换，也不推断丢失的费用或释放历史冻结。

## 两个不同的副本数

KEDA 的 HPA 写入 `ModelScale.spec.replicas`，它表示建议值；`ModelDeployment.spec.replicas` 才是控制面批准的执行值。`ModelScale` 暴露标准 `/scale` 子资源，状态返回实际应用的副本数和 Pod selector。Operator 不把建议值直接写到 Runtime。

成员 Agent 校验建议对象、ScaledObject、HPA 的 UID/owner、目标类型和健康条件，随后将建议随就绪观测上报控制面。无有效指标、目标不匹配或所有权不匹配时不执行新缩容。

- 扩容：控制面产生新的 desired-state revision，由 Agent 按 UID/RV 应用。旧的健康副本可以继续服务，不需要等待新增副本全部启动才接受请求。
- 缩容：持久记录自动操作，关闭新准入，等待 Gateway ACK 和在途归零，再批准 replicas 下降；确认应用和就绪后自动恢复入口。
- 排空尚未提交缩容时，如果有效建议恢复到当前副本数以上，取消该自动缩容并重新开放入口。
- 自动操作进行中，冲突的手动操作会被拒绝。状态和 desired-state 更新有数据库事务、版本和审计保护。

启用示例（合成配置）：

```yaml
autoscaling:
  managed: true
  minReplicas: 1
  maxReplicas: 3
  targetRunningRequests: 1
```

绑定入口时显式使用 `traffic_protocol: 2`。当前不支持自动 scale-to-zero；采用整池排空而不是逐 Pod 无中断 drain。希望缩容期间持续接收请求时，需要事先准备其他可用 Pool 的路由。

KEDA 支持通过标准 `/scale` 管理自定义资源，此处复用其 HPA 和指标计算，不实现另一套弹性算法。[KEDA 2.20 文档](https://keda.sh/docs/2.20/concepts/scaling-deployments/)

## 终止证明与遗留请求分开处理

Gateway 通过 Downward API 提供 Pod 身份。成员 Agent 读取该 Pod，并对 `/healthz` 返回的进程 incarnation 做校验，绑定实际 container ID、Node UID，并添加专用 finalizer，以保留正常删除期间的证据。

只有匹配容器的 kubelet `Terminated` 状态（包括同 Pod 重启的 lastState）、相同 Node UID、Ready 节点和新鲜 Node Lease 才能作为终止证据。Pod 不存在、节点失联或强制删除都不够；这也符合 Kubernetes 对强制删除不确认实际进程终止的说明。[Pod 生命周期](https://kubernetes.io/docs/concepts/workloads/pods/pod-lifecycle/)

终止证明提交后：

1. 该 Gateway 会话永久关闭，迟到 ACK 不得重新打开它。
2. 它的遗留在途数标记为 `-1`（未知），不是零。
3. v2 Envoy 对请求再次授权，检查短期 capability、Pool generation 和会话状态；已关闭会话的迟到请求仍会被拒绝。
4. 在 Pool 已关闭的条件下，等待终止栅栏之后的 Envoy `downstream_rq_active` 零值，以及完整、健康、新鲜的采集目标，才能清除未知在途状态。
5. 财务预占和未知用量保持原状态，不因容器终止而伪造 token 数或自动退款。

Capability 在进入 EPP/Runtime 前由 Envoy header-mutation 移除；客户端控制头仍由 Gateway 清理。实际配置和凭据只进入运行时 Secret，不进入 Bazel 输入或日志。

Node 不可达、证据缺失、时间不可信或指标缺失时继续阻止缩容。跨集群时必须保证时间同步、控制面可达、采集完整和 NetworkPolicy/mTLS 等网络边界。该方案不替代云主机断电/隔离的外部 fencing，也不声称已完成所有跨地域故障验收。

## 本地部署与验收

本地真实验证已通过：KEDA 升降副本、长流排空、独立测试 Gateway 的 kubelet 终止证明、终止栅栏后的 Envoy 空闲检查，以及恢复入口。演示后端仍是 Echo，不代表 GPU 模型验证。公共 Gateway 未被用于崩溃注入；临时验证资源已清理。

仅针对 Docker Desktop，所有应用构建仍使用 Bazel：

```bash
XSCOPE_BUILD_ARCH=arm64 bash tools/bazel-linux.sh images control-plane gateway cluster-agent operator
bazel run //tools:safe_scaling_local -- upgrade
bazel run //tools:safe_scaling_local -- bootstrap
bazel run //tools:safe_scaling_check -- --status
bazel run //tools:safe_scaling_check
```

`upgrade` 会短暂进入维护窗口，先排空 Gateway，再停旧控制面并在仓库外备份业务 schema。`resume` 仅用于确认上一轮已备份并停机后的恢复，不是跳过备份的普通入口。

`bootstrap` 保留原有入口，创建低资源成员 Agent 和独立受管演示模型；随机模型/项目标识、验证 Key 和成员凭据只存入 Secret。成员 Agent 新增请求为 10m CPU；演示入口 Envoy/EPP 合计 50m、每个 Echo Runtime 20m。验证结束的临时 Gateway 会清理。

`safe_scaling_check` 使用真实 KEDA、Operator、Envoy/EPP 和本地 Echo Runtime，观察建议与实际副本变化。崩溃测试仅对该工具创建的独立 Gateway Pod 发起带 UID/RV 前置条件、1 秒宽限期的删除，保留证据 finalizer，并等待 kubelet 确认终止；不使用 `--force`，不操作公共 Gateway。它不是 GPU 压测，也不会触发实际支付。

运维维护命令通过控制面容器内的 Rust CLI、显式环境开关和 stdin JSON 调用受限 Repository 方法，没有新增管理 HTTP 旁路或任意 SQL 执行接口。调用者需要 Kubernetes exec 权限；真实数据和返回凭据须捕获并直接写入 Secret。

补充：本地 Jaeger 曾因 256Mi 内存限制反复 OOM。当前维持 20m CPU 请求，调整内存预算和 Go 内存目标，保留原 Badger PVC、同步写入和 72 小时保留期，不通过清库恢复服务。
