# 控制台运营工作区

继续使用 React + Ant Design + TanStack Query；所有构建、类型检查和镜像打包经过 Bazel，不引入新的常驻 UI 服务。

2026-09-13 新增运行与恢复、发布历史/动作、证据核查/独立审批和事件重试；权限、验收及剩余边界见 [本轮说明](reliability-operations.md)。下方只读待核查界面的描述为历史版本。

## 使用入口

- `/#/dashboard`：本期已入账请求、Tokens、费用、ModelDeployment 就绪副本，以及有效密钥和已登记 Pool。存活检查不是推理链路就绪证明；未完成/未知用量不包含在已入账请求中。
- `/#/billing`：选择项目后显示账面、预占冻结、可用余额。待核查页仅向所属租户 owner 开放，支持 5 分钟/30 分钟/1 小时/24 小时时间边界和每页 20 条 keyset 分页；点击请求可复制 ID 查看详情。这里只读，不释放冻结或裁决供应商用量。
- `/#/observability`：打开既有 Grafana 指标和追踪看板，或输入 `x-trace-id` 响应头跳转 Jaeger Explore。不是 billing request ID。Grafana URL 可保存到当前浏览器，非本地环境不默认指向 localhost；仅接受不含凭证的 HTTP(S) 地址。Grafana 登录独立，不传递平台 token。
- `/#/deployments`：命名空间提交查询、名称/模型过滤、30 秒刷新、部署详情（资源声明/条件/内部 Endpoint）。安装时创建的 Echo/Envoy/EPP 不是 ModelDeployment，所以此列表可能为空。只读详情不会启动模型。

人工支付、退款和内部账单保留开发验证入口；**没有正式支付渠道和税务发票能力**。“对账演示”从平台支付记录构造模拟渠道数据，不能作为独立供应商对账凭证。选择项目只改变列表展示；对账演示仍按所选项目的租户进行。

## 新增后端读取契约

`GET /admin/v1/billing/accounts/{project}/position`

- 使用控制台会话鉴权，所属租户成员可读；未登录 401，跨租户 403。
- `balance_microunits`、`held_microunits`、`available_microunits` 均为十进制字符串，避免 JavaScript i64 精度损失。一个 minor currency unit 对应 1,000,000 microunits；CNY 一元对应 100,000,000 microunits。
- 同一投影行读取余额和冻结，`available = balance - held`，可用金额可能为负。暖读取不获取财务账户写锁；投影缺失时复用账户锁内首次初始化，不把缺失当零。该响应只是展示快照，不取代资金准入事务。

`GET /admin/v1/billing/accounts/{project}/pending-reservations`

- 所属租户 owner 可读，member 403。沿用内部接口的 `limit`（1–100）、`created_before` 和完整 keyset cursor；默认筛选超过 5 分钟的 dispatched 请求。
- 仅返回 ID、请求 ID、Key ID、状态、时间和字符串形式的冻结金额。内部 `spec`、`price`、`completion` 不直接暴露。
- 固定 cutoff 不代表数据库快照：并发结算会使记录消失。年龄和列表为空都不能证明财务已经结清。

## 验证与本地预览

```sh
bazel test //web/console:typecheck //web/console:format_test
bazel test //...
bazel run //tools:billing_protocol_smoke
bazel run //tools:console_preview
```

最后一个命令在 `http://localhost:4173` 提供 Bazel 产物的**只读预览**。先在 `http://localhost:30081` 登录；预览只向该固定本地地址转发现有会话 Cookie 的 GET 请求，不支持创建、更新、支付或删除。不访问数据库，不使用内部服务 token。尚未部署的新接口会显示读取失败，不是零余额。预览为开发进程，不替代 K8s NodePort 部署。

精确金额测试覆盖 i64 正负边界、微额、负余额以及 CNY/JPY/KWD 的不同 minor units。真实 PostgreSQL 回归测试覆盖匿名/跨租户拒绝、member 资金读取、owner 待核查读取、分页上限和内部字段脱敏。页面采用懒加载；资源更新或页面异常通过错误边界提供重新加载入口，不让整个控制台白屏。

## 本地部署验收（2026-09-06）

- 本轮全仓 19 个 Bazel 测试通过；真实 PostgreSQL 协议回归包含新增控制台接口的权限、分页参数和字段脱敏检查。之后的最后两处 UI 文案/无障碍调整再次通过类型与金额测试。
- Linux ARM64 Bazel release 控制面镜像 `sha256:a88d3e9eddf045e4d3151f67701f69c2c32ac73b82eb19cba4efd8dc2b1b07c0` 已部署，UI 仍由该服务提供，不增加常驻服务。停止旧控制面写入后保留私有备份 `.build/billing-backup-20260905T165416145667Z/xscope.sql`，没有重置身份、数据库或 PVC。
- 浏览器实际验证：总览布局；项目资金精确金额；待核查列表与只读详情；部署页空状态；Trace ID 跳转 Grafana。已有 Trace `3c68102e9f954e24bd64384d0e9d3ad8` 显示五服务、九个 span。桌面与约 600px 窄窗口做过视觉检查；没有宣称完成全部移动端或无障碍认证。
- 更新后的集群回归请求 `req-01a07283-e5a3-7683-b85b-46790d7a7ca6` 完成 reserve → dispatch → settled，只有一份平衡账本与事务事件，余额/冻结/Key/月度投影匹配源记录。该测试增加一条已结算的开发用量记录；原有未知用量冻结未改变。15 个平台 Pod 均就绪，Gateway readiness 返回 200。
- 这些是本地 Echo Runtime 功能验证，不是 GPU 吞吐或生产容量认证。临时只读预览服务已停止，日常访问使用现有 NodePort `http://localhost:30081/`。
