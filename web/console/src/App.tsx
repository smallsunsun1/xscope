import { t, useI18n } from "./i18n";
import { useQuery } from "@tanstack/react-query";
import { Alert, Avatar, Badge, Button, Drawer, Dropdown, Layout, Menu, Select, Spin, Tooltip } from "antd";
import type { MenuProps } from "antd";
import {
  Activity,
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
  UsersRound,
} from "lucide-react";
import { lazy, Suspense, useEffect, useState } from "react";
import { api } from "./api";
import { PageBoundary } from "./components";

const APIKeysPage = lazy(() => import("./pages/APIKeysPage").then((module) => ({ default: module.APIKeysPage })));
const BillingPage = lazy(() => import("./pages/BillingPage").then((module) => ({ default: module.BillingPage })));
const DashboardPage = lazy(() => import("./pages/DashboardPage").then((module) => ({ default: module.DashboardPage })));
const DeploymentsPage = lazy(() => import("./pages/DeploymentsPage").then((module) => ({ default: module.DeploymentsPage })));
const ModelsPage = lazy(() => import("./pages/ModelsPage").then((module) => ({ default: module.ModelsPage })));
const ProjectsPage = lazy(() => import("./pages/ProjectsPage").then((module) => ({ default: module.ProjectsPage })));
const UsersPage = lazy(() => import("./pages/UsersPage").then((module) => ({ default: module.UsersPage })));
const RoutingPage = lazy(() => import("./pages/RoutingPage").then((module) => ({ default: module.RoutingPage })));
const ObservabilityPage = lazy(() => import("./pages/ObservabilityPage").then((module) => ({ default: module.ObservabilityPage })));

const { Content, Sider } = Layout;

const pageNames = () => ({
  dashboard: t("平台总览"),
  projects: t("项目"),
  keys: "API Keys",
  models: t("模型与价格"),
  deployments: t("模型部署"),
  routing: t("流量路由"),
  billing: t("用量与计费"),
  users: t("用户与成员"),
  observability: t("监控与追踪"),
} as const);

type Page = keyof ReturnType<typeof pageNames>;

const menuItems = (): MenuProps["items"] => [
  { key: "dashboard", icon: <LayoutDashboard size={18} />, label: t("平台总览") },
  { type: "group", label: t("资源管理"), children: [
    { key: "projects", icon: <FolderKanban size={18} />, label: t("项目") },
    { key: "keys", icon: <KeyRound size={18} />, label: "API Keys" },
    { key: "models", icon: <Layers3 size={18} />, label: t("模型与价格") },
    { key: "deployments", icon: <Boxes size={18} />, label: t("模型部署") },
    { key: "routing", icon: <Layers3 size={18} />, label: t("流量路由") },
  ] },
  { type: "group", label: t("平台运营"), children: [
    { key: "observability", icon: <Activity size={18} />, label: t("监控与追踪") },
    { key: "users", icon: <UsersRound size={18} />, label: t("用户与成员") },
    { key: "billing", icon: <ReceiptText size={18} />, label: t("用量与计费") },
  ] },
];

function pageFromHash(): Page {
  const candidate = window.location.hash.replace(/^#\/?/, "") as Page;
  return Object.hasOwn(pageNames(), candidate) ? candidate : "dashboard";
}

function Logo() {
  useI18n();
  return (
    <div className="brand">
      <span className="brand-mark"><Command size={20} strokeWidth={2.6} /></span>
      <span><strong>XScope</strong><small>MODEL PLATFORM</small></span>
    </div>
  );
}

function Navigation({ page, navigate }: { page: Page; navigate: (page: Page) => void }) {
  useI18n();
  return (
    <>
      <Logo />
      <Menu
        className="side-menu"
        mode="inline"
        theme="dark"
        selectedKeys={[page]}
        items={menuItems()}
        onClick={({ key }) => navigate(key as Page)}
      />
      <div className="sidebar-bottom">
        <button onClick={() => navigate("observability")}><CircleHelp size={17} /><span>{t("监控访问指南")}</span></button>
        <div className="environment-card">
          <div><Badge status="default" /><strong>{t("当前控制台")}</strong></div>
          <span>{window.location.host}</span>
        </div>
      </div>
    </>
  );
}

export function ConsoleApp() {
  const { locale, setLocale } = useI18n();
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
    routing: <RoutingPage />,
    billing: <BillingPage />,
    users: <UsersPage />,
    observability: <ObservabilityPage />,
  }[page];
  const accountName = session.data?.username || session.data?.email || t("平台用户");
  const accountEmail = session.data?.email || t("已通过 OIDC 登录");
  const avatarText = accountName.trim().slice(0, 1).toUpperCase() || t("用");

  return (
    <Layout className="app-layout">
      <Sider width={244} className="app-sidebar" theme="dark">
        <Navigation page={page} navigate={navigate} />
      </Sider>
      <Layout className="main-layout">
        <header className="topbar">
          <div className="topbar-left">
            <Button aria-label={t("打开导航菜单")} className="mobile-menu-button" type="text" icon={<MenuIcon size={20} />} onClick={() => setMobileNav(true)} />
            <span>{t("控制台")}</span><i>/</i><strong>{pageNames()[page]}</strong>
          </div>
          <div className="topbar-actions">
            <Select
              aria-label={t("语言")}
              className="language-select"
              value={locale}
              onChange={setLocale}
              popupMatchSelectWidth={false}
              options={[{ value: "zh-CN", label: "简体中文" }, { value: "en-US", label: "English" }]}
            />
            <Tooltip title={health.isSuccess ? t("控制面连接正常") : t("控制面不可用")}>
              <div className={`connection-pill ${health.isSuccess ? "connected" : "disconnected"}`}>
                <span />{health.isPending ? t("正在连接") : health.isSuccess ? t("控制面已连接") : t("连接中断")}
              </div>
            </Tooltip>
            <Dropdown menu={{
              items: [
                { key: "identity", label: accountEmail, disabled: true },
                { key: "logout", label: t("退出登录") },
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
          <div className="content-body">{session.isError && <Alert className="page-alert" type="error" showIcon title={t("无法验证控制台会话")} description={t("请刷新登录状态。当前页面数据可能不可用。")} action={<Button href="/">{t("重新登录")}</Button>} />}<PageBoundary key={page}><Suspense fallback={<div className="page-loader"><Spin size="large" /></div>}>{content}</Suspense></PageBoundary></div>
          <footer><span>XScope Console · Operations workspace</span><span>Rust control plane · Kubernetes</span></footer>
        </Content>
      </Layout>
      <Drawer className="mobile-navigation" placement="left" size={264} open={mobileNav} onClose={() => setMobileNav(false)} closable={false}>
        <Navigation page={page} navigate={navigate} />
      </Drawer>
    </Layout>
  );
}
