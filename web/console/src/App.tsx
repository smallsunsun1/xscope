import { useQuery } from "@tanstack/react-query";
import { Avatar, Badge, Button, Drawer, Dropdown, Layout, Menu, Spin, Tooltip } from "antd";
import type { MenuProps } from "antd";
import {
  Bell,
  Boxes,
  ChevronDown,
  CircleHelp,
  Command,
  FolderKanban,
  KeyRound,
  Layers3,
  LayoutDashboard,
  Menu as MenuIcon,
  ReceiptText,
  Settings,
  UsersRound,
} from "lucide-react";
import { lazy, Suspense, useEffect, useState } from "react";
import { api } from "./api";

const APIKeysPage = lazy(() => import("./pages/APIKeysPage").then((module) => ({ default: module.APIKeysPage })));
const BillingPage = lazy(() => import("./pages/BillingPage").then((module) => ({ default: module.BillingPage })));
const DashboardPage = lazy(() => import("./pages/DashboardPage").then((module) => ({ default: module.DashboardPage })));
const DeploymentsPage = lazy(() => import("./pages/DeploymentsPage").then((module) => ({ default: module.DeploymentsPage })));
const ModelsPage = lazy(() => import("./pages/ModelsPage").then((module) => ({ default: module.ModelsPage })));
const ProjectsPage = lazy(() => import("./pages/ProjectsPage").then((module) => ({ default: module.ProjectsPage })));
const UsersPage = lazy(() => import("./pages/UsersPage").then((module) => ({ default: module.UsersPage })));

const { Content, Sider } = Layout;

const pageNames = {
  dashboard: "平台总览",
  projects: "项目",
  keys: "API Keys",
  models: "模型与价格",
  deployments: "模型部署",
  billing: "用量与计费",
  users: "用户与成员",
} as const;

type Page = keyof typeof pageNames;

const menuItems: MenuProps["items"] = [
  { key: "dashboard", icon: <LayoutDashboard size={18} />, label: "平台总览" },
  { type: "group", label: "资源管理", children: [
    { key: "projects", icon: <FolderKanban size={18} />, label: "项目" },
    { key: "keys", icon: <KeyRound size={18} />, label: "API Keys" },
    { key: "models", icon: <Layers3 size={18} />, label: "模型与价格" },
    { key: "deployments", icon: <Boxes size={18} />, label: "模型部署" },
  ] },
  { type: "group", label: "平台运营", children: [
    { key: "users", icon: <UsersRound size={18} />, label: "用户与成员" },
    { key: "billing", icon: <ReceiptText size={18} />, label: "用量与计费" },
  ] },
];

function pageFromHash(): Page {
  const candidate = window.location.hash.replace(/^#\/?/, "") as Page;
  return candidate in pageNames ? candidate : "dashboard";
}

function Logo() {
  return (
    <div className="brand">
      <span className="brand-mark"><Command size={20} strokeWidth={2.6} /></span>
      <span><strong>XScope</strong><small>MODEL PLATFORM</small></span>
    </div>
  );
}

function Navigation({ page, navigate }: { page: Page; navigate: (page: Page) => void }) {
  return (
    <>
      <Logo />
      <Menu
        className="side-menu"
        mode="inline"
        theme="dark"
        selectedKeys={[page]}
        items={menuItems}
        onClick={({ key }) => navigate(key as Page)}
      />
      <div className="sidebar-bottom">
        <button><CircleHelp size={17} /><span>帮助与文档</span></button>
        <button><Settings size={17} /><span>平台设置</span><em>即将提供</em></button>
        <div className="environment-card">
          <div><Badge status="processing" /><strong>本地环境</strong></div>
          <span>Asia/Shanghai · local</span>
        </div>
      </div>
    </>
  );
}

export function ConsoleApp() {
  const [page, setPage] = useState<Page>(pageFromHash);
  const [mobileNav, setMobileNav] = useState(false);
  const health = useQuery({
    queryKey: ["control-health"],
    queryFn: api.health,
    refetchInterval: 15_000,
    retry: false,
  });
  const session = useQuery({
    queryKey: ["console-session"],
    queryFn: api.session,
    retry: false,
  });

  useEffect(() => {
    const onHashChange = () => setPage(pageFromHash());
    window.addEventListener("hashchange", onHashChange);
    return () => window.removeEventListener("hashchange", onHashChange);
  }, []);

  const navigate = (next: Page) => {
    window.location.hash = `/${next}`;
    setPage(next);
    setMobileNav(false);
  };
  const content = {
    dashboard: <DashboardPage navigate={(next) => navigate(next as Page)} />,
    projects: <ProjectsPage />,
    keys: <APIKeysPage />,
    models: <ModelsPage />,
    deployments: <DeploymentsPage />,
    billing: <BillingPage />,
    users: <UsersPage />,
  }[page];
  const accountName = session.data?.username || session.data?.email || "平台用户";
  const accountEmail = session.data?.email || "已通过 OIDC 登录";
  const avatarText = accountName.trim().slice(0, 1).toUpperCase() || "用";

  return (
    <Layout className="app-layout">
      <Sider width={244} className="app-sidebar" theme="dark">
        <Navigation page={page} navigate={navigate} />
      </Sider>
      <Layout className="main-layout">
        <header className="topbar">
          <div className="topbar-left">
            <Button className="mobile-menu-button" type="text" icon={<MenuIcon size={20} />} onClick={() => setMobileNav(true)} />
            <span>控制台</span><i>/</i><strong>{pageNames[page]}</strong>
          </div>
          <div className="topbar-actions">
            <Tooltip title={health.isSuccess ? "控制面连接正常" : "控制面不可用"}>
              <div className={`connection-pill ${health.isSuccess ? "connected" : "disconnected"}`}>
                <span />{health.isSuccess ? "控制面已连接" : "连接中断"}
              </div>
            </Tooltip>
            <Tooltip title="通知"><Button type="text" shape="circle" icon={<Bell size={18} />} /></Tooltip>
            <Dropdown menu={{
              items: [
                { key: "identity", label: accountEmail, disabled: true },
                { key: "logout", label: "退出登录" },
              ],
              onClick: ({ key }) => {
                if (key === "logout") window.location.assign("/oauth2/sign_out?rd=/");
              },
            }}>
              <button className="account-menu">
                <Avatar size={32}>{avatarText}</Avatar>
                <span><strong>{accountName}</strong><small>{accountEmail}</small></span>
                <ChevronDown size={14} />
              </button>
            </Dropdown>
          </div>
        </header>
        <Content className="app-content">
          <main><Suspense fallback={<div className="page-loader"><Spin size="large" /></div>}>{content}</Suspense></main>
          <footer><span>XScope Console · v0.1.0</span><span>控制面 API v1alpha1</span></footer>
        </Content>
      </Layout>
      <Drawer className="mobile-navigation" placement="left" size={264} open={mobileNav} onClose={() => setMobileNav(false)} closable={false}>
        <Navigation page={page} navigate={navigate} />
      </Drawer>
    </Layout>
  );
}
