import { useQuery } from "@tanstack/react-query";
import { Avatar, Badge, Button, Drawer, Dropdown, Layout, Menu, Tooltip } from "antd";
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
  Settings,
} from "lucide-react";
import { useEffect, useState } from "react";
import { api } from "./api";
import { APIKeysPage } from "./pages/APIKeysPage";
import { DashboardPage } from "./pages/DashboardPage";
import { DeploymentsPage } from "./pages/DeploymentsPage";
import { ModelsPage } from "./pages/ModelsPage";
import { ProjectsPage } from "./pages/ProjectsPage";

const { Content, Sider } = Layout;

const pageNames = {
  dashboard: "平台总览",
  projects: "项目",
  keys: "API Keys",
  models: "模型与价格",
  deployments: "模型部署",
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
  }[page];

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
            <Dropdown menu={{ items: [{ key: "profile", label: "账户信息", disabled: true }, { key: "logout", label: "退出登录", disabled: true }] }}>
              <button className="account-menu">
                <Avatar size={32}>管</Avatar>
                <span><strong>平台管理员</strong><small>admin@local</small></span>
                <ChevronDown size={14} />
              </button>
            </Dropdown>
          </div>
        </header>
        <Content className="app-content">
          <main>{content}</main>
          <footer><span>XScope Console · v0.1.0</span><span>控制面 API v1alpha1</span></footer>
        </Content>
      </Layout>
      <Drawer className="mobile-navigation" placement="left" width={264} open={mobileNav} onClose={() => setMobileNav(false)} closable={false}>
        <Navigation page={page} navigate={navigate} />
      </Drawer>
    </Layout>
  );
}
