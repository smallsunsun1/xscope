# 多集群部署边界

## 本地形态

Docker Desktop overlay 为节省资源，把管理面和数据面共同放在 `xscope-system` namespace：

```text
OAuth2 Proxy -> Control Plane + Console -> Kubernetes API
       |              |
    Keycloak ------ PostgreSQL

Client -> Pingora Gateway -> development Runtime
                         Operator -> ModelDeployment workloads
```

PostgreSQL、Keycloak、OAuth2 Proxy、Control Plane、Pingora、Operator 和开发 Runtime 均为
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
- 每个数据面配置不可变的 `cluster_id`、`region` 和能力标签；Operator 把这些字段写进
  workload 标签和 `ModelDeployment.status`。
- Pingora 负责租户鉴权、地域、稳定版/金丝雀版本和故障转移；同一模型版本内部的 Pod
  选择交给 KServe/llm-d EPP，避免在全局网关重复实现 KV-cache 索引。
- stable 与 canary 使用不同 InferencePool。请求头规则先在 Pingora 完成授权和清洗，再选择
  pool；进入 pool 后由 prefix-cache/load-aware scheduler 选择具体 GPU endpoint。
- PostgreSQL 是管理面唯一写主；推理请求热路径不访问 PostgreSQL。网关只消费已签名配置
  快照并保留 last-known-good。
- 本地 overlay 使用 NodePort 方便验证；生产入口使用 Gateway API、TLS 和 NetworkPolicy，
  不复用本地明文凭据。

后续成员集群安装包应只包含 CRD、Agent、Operator、Pingora 和可选的 KServe/llm-d 组件；
PostgreSQL、Keycloak 与 Console 不随每个推理集群重复部署。
