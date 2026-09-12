# 待核查请求：证据与双人审批

2026-09-06：本页描述凭真实用量证据补结算的路径。另已增加独立的 [损失豁免流程](usage-delivery.md#未决资金闭环)：两名财务审核员可对确实无法恢复用量、已终止的请求批准 `waived`，释放预占但不编造 token/费用。客户不能自助豁免；没有审批仍保持冻结。

本轮落地第 1 项的后端闭环：人工提交原始用量凭证 → 独立审批 → 幂等补结算 → 事务审计。
没有自动估价或按超时解冻，不修改现有未知用量记录。所有业务读写使用 SeaORM；迁移 000008 的 PostgreSQL 专用触发器仅保护证据与审计。

## 权限与证据真实性

- 提交人必须是项目所属租户 owner，且控制台身份鉴权必须开启。`XSCOPE_CONSOLE_AUTH=disabled` 不允许提交或审批。
- 审批人必须在部署配置 `XSCOPE_BILLING_REVIEWER_SUBJECTS` 中显式登记，值为逗号分隔的 **OIDC external_subject**，不是可修改的显示名或邮箱。默认空，全部拒绝审批。可通过现有 `/admin/v1/session` 确认自己的 `external_subject`。
- 该配置授予跨项目财务核查读取与审批权限，应仅配置平台财务人员。租户 owner 无法通过租户成员管理自行获得这项权限。
- 提交人和审批人的稳定平台用户 ID 必须不同；即使提交人也在审批名单中，仍禁止自批。第一版只要求一名独立审批人，不等于防串通制度或正式财务合规认证。
- 证据是最多 65,536 字节的 UTF-8 原始用量回执/日志，附来源、来源请求 ID、说明和最终用量。不要上传 API Key、Cookie、提示词或个人敏感信息；来源地址不被自动访问。
- SHA-256 覆盖规范化后的完整提交及项目/预占身份，保证审批绑定的版本一致，**不证明供应商真实性**。审批人仍须在 Runtime/供应商侧核对请求关联、最终状态、时间与 token 用量；没有可靠证据就不批准。供应商签名验证、自动取证适配器与外部不可变存储尚未接入。

## HTTP 合同

以下均使用现有控制台登录会话，不接受 Gateway 内部 token 作为审批凭据。浏览器经 OAuth2 Proxy 访问 `/api/admin/v1`；不能将可信身份 Header 入口直接暴露给互联网。

| 方法与 `/admin/v1` 路径 | 用途 |
| --- | --- |
| `POST /billing/accounts/{project}/reservations/{reservation}/reviews` | 提交一份不可修改的证据 |
| `GET /billing/accounts/{project}/reservations/{reservation}/reviews?limit=20&after_id=...` | 分页读取历史摘要；limit 1–100，不返回原文 |
| `GET /billing/accounts/{project}/reviews/{id}` | 原始证据、决定、提交与决定审计 |
| `POST /billing/accounts/{project}/reviews/{id}/decision` | 独立财务审批或拒绝 |

提交示例（占位身份必须替换为确有证据的请求，不能照抄用于现有冻结）：

```json
{
  "id": "review-unique-id",
  "source": "runtime-cluster-a",
  "source_request_id": "provider-request-id",
  "document": "原始最终用量回执，不是自行推测的数据",
  "explanation": "请求关联与最终用量的核查说明",
  "usage": {
    "input_tokens": 100,
    "output_tokens": 20,
    "latency_ms": 500,
    "endpoint_id": "endpoint-a",
    "region": "local",
    "status": "succeeded"
  }
}
```

服务器返回 `evidence_sha256`。审批人读取完整证据并确认后提交：

```json
{
  "evidence_sha256": "读取到的完整64位SHA-256",
  "action": "approve",
  "reason": "独立核对凭证来源、请求ID和最终用量后的结论"
}
```

`action=reject` 必须同样给出说明，拒绝只终结这份证据，不解冻资金。更正证据使用新 ID 重新提交，历史不可覆盖。没有新的 UI 审批按钮，本轮先验收后端合同。

## 状态、事务与重试

- 只允许对 `dispatched` 请求提交新证据；该状态也可能仍在推理，年龄不是完成凭证。
- 证据状态：`submitted → settled` 或 `submitted → rejected`，仅一次终态决定。同 ID、同提交人、同内容的提交重试返回原记录；改变内容或目标返回 409。
- 同决定、同审批人的审批重试返回原结果；更改原因、动作或审批人返回 409。不要在网络超时后生成新的批准请求来尝试补偿。
- 账户锁与 Gateway 结算共用，审批调用同一个 `settle_locked`。原冻结价格和输入/输出上限仍生效，审批人不能传金额或改价格。超限返回 409，证据保持 submitted、资金继续冻结，不强制透支。
- 结算使用原请求 identity；用量、账本、账户/Key/月度投影、预占终态、审批决定、两类事务事件和审计同时提交。审计或 Outbox 失败，全部回滚。
- 如果 Gateway 已用完全相同的 completion 结算，批准不会再扣一次；任何 completion 字段不一致均冲突，保留证据，不能覆盖已入账用量。真实零用量也需要凭证和独立审批，产生用量/审批事件但不制造零额账本分录。
- 记账月份沿用当前结算合同，为补结算提交的 UTC 月，而不是凭证声称的历史月份。历史账单重开/更正另行设计。
- 列表按 ID keyset 分页，不是提交时间排序或跨页数据库快照；摘要分页最多 100 条。详情在同一只读快照内读取证据与审计。

## 审计和存储边界

`billing_reviews` 保留原始提交、SHA-256、提交人、一次终态决定与时间；`billing_review_audit` 对每份证据保留提交和决定两条追加记录，包括稳定用户 ID、OIDC subject、证据 hash、审批动作和原因。事务 Outbox 增加 `review.submitted` / `review.settled` / `review.rejected`，不包含原始文档。

普通 UPDATE/DELETE/TRUNCATE 不能修改证据或删除审计；不提供覆盖、删除和自动 down migration。数据库管理员仍能关闭触发器或删除表，因此这不是防 DBA 的 WORM 存储。HTTP 鉴权失败、格式错误和冲突不是财务状态变更，不写入这张业务审计表；全平台安全审计属于后续阶段。

## 验证

```sh
bazel test //...
bazel run //tools:billing_protocol_smoke
```

临时 PostgreSQL 验证匿名/member/跨租户拒绝、平台审批授权、自批拒绝、证据重放冲突、原文/审计读取、审核 hash、注入审计故障后的全事务回滚、八个并发批准只扣一次、不同证据与另一控制面的 Gateway 结算竞态、未知/超限冻结、凭证零用量、数据库触发器、keyset 分页以及进程重启后的决定重试。临时数据库被移除，不创建或修改本地集群的核查凭证。

Gateway 本地 WAL/归档方案已退役，改为 [HTTP 投递与损失豁免](usage-delivery.md)。历史证据不删除；生产事件消费者的领取、退避、幂等副作用和 ACK 见 [后台工作流](backend-workflows.md)。

## 本地部署验收（2026-09-06）

- 21 个全仓 Bazel 测试通过，临时 PostgreSQL 的完整计费协议与新增核查回归通过，临时测试进程/数据库已清理。
- Bazel Linux ARM64 release 镜像 `sha256:3dbdfd5a9b2ca36c0bb8a755ec043628783578366115e4d8c36f15ca41114e67` 已部署为 `control-plane-55d8d8657f-wb5j8`。迁移表与 4 个保护触发器已验证；CPU request 仍为 20m，不增加服务。
- 停止旧控制面写入后的私有备份：`.build/billing-backup-20260905T231646936902Z/xscope.sql`。没有数据重置；没有为测试授予真实审批权限或提交凭证。
- 已部署的新接口匿名请求返回 401。开发推理 `req-01a073dd-663e-7ed1-84bc-8966eacec828` 正常预占/派发/结算，账本平衡且投影一致。该检查增加一条已结算的 Echo 用量；不是 GPU 验证。
- 原未知用量请求 `req-01a071b8-fe92-74c0-9dcd-d9482c3e7db2` 仍为 dispatched，冻结 33,792,000 microunits 未变；15 个平台 Pod 就绪，Gateway readiness 返回 200。
