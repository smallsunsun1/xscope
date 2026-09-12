# 多集群部署边界

> 2026-09-06：下方“生产多集群形态”是目标架构，不是已全部联通的现状。当前新增的可执行协议如下；不能把摘要哈希称为数字签名，也不能说预付费推理完全不访问数据库。

## 已实现的出站协议

- 管理面以 SeaORM 保存集群身份、不可变 revision 和 ACK。管理员由 Secret 中的 `XSCOPE_PLATFORM_ADMIN_SUBJECTS` 指定 OIDC 不可变 subject；默认空集合不授予跨租户管理权。普通租户 owner 无权注册成员集群。
- `/admin/v1/clusters` 注册时返回一次性可见的随机凭据，数据库只保留哈希。凭据期限 1–90 天；轮换使用 `expected_epoch`，撤销/到期立即拒绝旧请求，Secret 更新不要求重建镜像。
- `PUT /admin/v1/clusters/{id}/desired-state` 使用 `expected_version` CAS；每次提交最多 100 个不同名称的部署、256 KiB。摘要覆盖规范 JSON；传输依赖 HTTPS + Bearer，不是签名快照或 mTLS。
- Agent 从私有文件 `XSCOPE_CLUSTER_PULL_CONFIG_FILE` 读取 `cluster_id`、固定 `namespace`、`control_url`、`credential_file`。只有显式开发开关允许 loopback / `.svc` HTTP。配置和凭据全部通过 Secret 挂载，不放 ConfigMap。
- Agent 每 15 秒出站 poll（失败最多退避 60 秒）。控制面发放 60 秒 delivery lease；ACK/NACK 绑定版本、摘要、凭据状态和 lease。ACK 只表示 Kubernetes 接受资源，不代表模型 Ready。
- Agent 用 `kube-leader-election` 在受管命名空间选主；续约失败立即取消本地 writer。写 CRD 使用 resourceVersion，删除额外要求显式 UID。原有其他所有者资源不接管；省略资源不隐式删除。更新到更高 revision 时，尚未执行的删除 tombstone 必须继续携带，直到观察到 ACK。
- 出站模式禁用旧入站 CRUD API。当前本地集群仍用原入站模式，避免将现有资源自动接管；注册真实成员需另行提供目标集群和命名空间。参见 `deploy/k8s/examples/member-agent-rbac.yaml` 的最小权限示例。
- 已用真实 Agent/两个控制面进程与隔离 PostgreSQL 验证协议，Kubernetes API 是 HTTP fixture。尚未完成第二个真实集群、分区期间已发送 K8s 请求的强 fencing、自动地域容灾或 mTLS 联调。
- 金额预占与结算仍经控制面进入 PostgreSQL 同步事务。不能在管理面分区时无条件放行预付费请求；last-known-good 路由配置不替代金额准入。

## 本地形态

Docker Desktop overlay 为节省资源，把管理面和数据面共同放在 `xscope-system` namespace：

```text
OAuth2 Proxy -> Rust Control Plane + Console -> Rust Cluster Agent -> Kubernetes API
       |              |
    Keycloak ------ PostgreSQL <------ usage

Client -> Pingora Gateway -> development Runtime
                         Operator -> ModelDeployment workloads
```

PostgreSQL、Redis、Keycloak、OAuth2 Proxy、Rust Control Plane、Rust Cluster Agent、Pingora、Operator 和开发 Runtime 均为
单副本。模型副本和智能调度组件不常驻空闲集群，只在创建对应模型服务时消耗资源。

## 生产多集群形态

管理集群只保存全局状态，推理集群按 region/zone 独立运行数据面：

```text
management cluster
  Console / OIDC / Control Plane / PostgreSQL
                    |
            signed desired-state stream
             +------+------+
             |             |
inference cluster cn   inference cluster sg
  Agent / Operator       Agent / Operator
  Pingora / AI Gateway   Pingora / AI Gateway
  InferencePools         InferencePools
```

关键约束：

- 管理面不保存成员集群的长期高权限 kubeconfig。成员集群 Agent 使用短期凭据建立出站连接，
  拉取带版本和签名的期望状态。
- 当前本地版已把 Kubernetes client 和 RBAC 完全收敛到 Rust Cluster Agent；Rust 控制面只持有
  Agent 的内部服务令牌。生产版将同一 API 替换为 mTLS 出站隧道和按 `cluster_id` 注册的连接。
- 每个数据面配置不可变的 `cluster_id`、`region` 和能力标签；Operator 把这些字段写进
  workload 标签和 `ModelDeployment.status`。
- Pingora 负责租户鉴权、地域、稳定版/金丝雀版本和故障转移；同一模型版本内部的 Pod
  选择交给 KServe/llm-d EPP，避免在全局网关重复实现 KV-cache 索引。
- stable 与 canary 使用不同 InferencePool。请求头规则先在 Pingora 完成授权和清洗，再选择
  pool；进入 pool 后由 prefix-cache/load-aware scheduler 选择具体 GPU endpoint。
- PostgreSQL 是管理面唯一写主；推理请求热路径不访问 PostgreSQL。本地初版由网关轮询服务
  鉴权的策略接口并保留 last-known-good；生产成员集群应只消费签名、版本化的配置快照。
- 本地 overlay 使用 NodePort 方便验证；生产入口使用 Gateway API、TLS 和 NetworkPolicy，
  不复用本地明文凭据。

后续成员集群安装包应只包含 CRD、Agent、Operator、Pingora 和可选的 KServe/llm-d 组件；
PostgreSQL、Keycloak 与 Console 不随每个推理集群重复部署。
