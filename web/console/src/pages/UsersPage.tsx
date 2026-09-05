import { useMutation, useQuery, useQueryClient } from "@tanstack/react-query";
import { App, Select, Table, Tag } from "antd";
import { ShieldCheck, UserRound } from "lucide-react";
import { api, errorMessage } from "../api";
import { PageHeader, ResourceEmpty } from "../components";
import { formatDate } from "../format";
import type { PlatformUser, TenantMembership } from "../types";

export function UsersPage() {
  const { message } = App.useApp();
  const queryClient = useQueryClient();
  const users = useQuery({ queryKey: ["users"], queryFn: api.users, retry: false });
  const updateRole = useMutation({
    mutationFn: ({ userID, membership, role }: {
      userID: string;
      membership: TenantMembership;
      role: "owner" | "member";
    }) => api.updateMembership(userID, membership.tenant_id, role),
    onSuccess: async () => {
      await queryClient.invalidateQueries({ queryKey: ["users"] });
      message.success("租户角色已更新");
    },
    onError: (error) => message.error(errorMessage(error)),
  });

  return (
    <>
      <PageHeader
        eyebrow="Identity"
        title="用户与成员"
        description="OIDC 负责登录，Rust 控制面将平台账号和租户成员关系通过 SeaORM 保存到 PostgreSQL。"
      />
      <section className="panel table-panel">
        <div className="table-toolbar">
          <strong>已同步的平台用户</strong>
          <Tag icon={<ShieldCheck size={13} />}>租户隔离</Tag>
        </div>
        <Table<PlatformUser>
          rowKey="id"
          loading={users.isLoading}
          dataSource={users.data}
          pagination={{ pageSize: 8, hideOnSinglePage: true }}
          locale={{
            emptyText: (
              <ResourceEmpty
                title="暂无可管理用户"
                description="用户首次通过 OIDC 登录后会同步到平台；本地环境会自动加入默认租户。"
              />
            ),
          }}
          columns={[
            {
              title: "用户",
              render: (_, user) => (
                <div className="primary-cell">
                  <span className="table-icon blue"><UserRound size={16} /></span>
                  <span><strong>{user.username}</strong><small>{user.email || user.external_subject}</small></span>
                </div>
              ),
            },
            {
              title: "租户角色",
              render: (_, user) => (
                <div className="membership-list">
                  {user.memberships.map((membership) => (
                    <div key={membership.tenant_id} className="membership-item">
                      <code className="soft-code">{membership.tenant_id}</code>
                      <Select
                        size="small"
                        value={membership.role}
                        loading={updateRole.isPending}
                        style={{ width: 100 }}
                        options={[
                          { value: "owner", label: "Owner" },
                          { value: "member", label: "Member" },
                        ]}
                        onChange={(role: "owner" | "member") => updateRole.mutate({ userID: user.id, membership, role })}
                      />
                    </div>
                  ))}
                </div>
              ),
            },
            { title: "状态", width: 110, dataIndex: "status", render: (status: string) => <Tag color={status === "active" ? "success" : "default"}>{status}</Tag> },
            { title: "最近登录", width: 170, dataIndex: "last_login_at", render: formatDate },
          ]}
        />
      </section>
    </>
  );
}
