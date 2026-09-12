# 仓库发布与凭据安全

真实账号、密码、RAM AK、接入点域名/别名和私有部署配置都不属于代码。`AGENTS.md` 只记录规则，不记录这些值。忽略文件、Base64、Kubernetes Secret 都不能替代权限控制；有权读取集群 Secret 的主体仍可读取明文。生产环境需限制 RBAC 并启用静态加密/外部 Secret 管理。

## 本地部署

`tools/deploy-local.sh` 通过 `bazel run //tools:local_secrets` 首次随机生成数据库、Keycloak、OAuth、内部通信和开发 API Key 的 Secret；后续保留已有值，不自动轮换。若 PostgreSQL PVC 已存在但对应 Secret 丢失，部署拒绝生成不匹配的新密码。

不要把真实值放在命令行、shell 历史、环境转储、Git 文件或 Bazel action 环境中。使用运行时隐藏输入、stdin 传递 Secret，并捕获输出；不要对 Secret 使用产生 `last-applied-configuration` 副本的客户端 apply。私有临时文件和备份应存放在仓库外，目录 0700、文件 0600。

Gateway WAL/OSS 归档和配置工具已随旧模式移除。已有归档 Secret、云对象和历史 PVC 不自动删除或重新配置；长期保留、凭据轮换与资产下线仍需独立授权。后续如给其他业务接入对象存储，应按实际用途设计，不恢复 Gateway 的旧模式。

## 发布前扫描

```sh
bazel run //tools/security:scan -- --working-only
bazel run //tools/security:scan
```

仓库提供 `.githooks/pre-push`，可用 `git config --local core.hooksPath .githooks` 启用本地推送门禁（先确认没有现有自定义 hooks）。它在推送前扫描工作区及全部可达历史，命中或工具失败都阻止推送。Hooks 是本地防误操作措施，不是无法绕过的服务端安全控制；GitHub 仍应开启 secret scanning/push protection。

基于校验和锁定的 Gitleaks；第一条检查所有 Git 跟踪/未忽略文件，第二条再检查全部可达 Git 历史。只输出文件、行号、规则和提交哈希，不输出命中内容。报告与工作区快照在仓库外短期创建并清理。不接受行内 `gitleaks:allow` 绕过；唯一精确例外是已核实的公开容器镜像摘要。

扫描通过不等于绝对安全，还需人工审查。旧开发凭据曾进入 Git 历史，删除当前文件并不能消除历史记录。发布前须轮换受影响凭据，再经明确授权清理历史，或创建不包含旧历史的干净发布副本。不要擅自改写历史、强推或重置现有账号。聊天中提供过的 RAM 凭据也建议轮换；新的密钥只通过隐藏输入配置，不继续粘贴到聊天中。
