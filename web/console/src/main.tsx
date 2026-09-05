import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { App as AntApp, ConfigProvider } from "antd";
import zhCN from "antd/locale/zh_CN";
import enUS from "antd/locale/en_US";
import "antd/dist/reset.css";
import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { ConsoleApp } from "./App";
import { useI18n } from "./i18n";
import "./styles.css";

const queryClient = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 10_000,
      refetchOnWindowFocus: false,
    },
  },
});

const root = document.getElementById("root");
if (!root) throw new Error("missing #root element");

function LocalizedConsole() {
  const { locale } = useI18n();
  return (
      <ConfigProvider
        locale={locale === "zh-CN" ? zhCN : enUS}
        theme={{
          token: {
            colorPrimary: "#168f70",
            colorInfo: "#168f70",
            colorLink: "#147b63",
            colorBgLayout: "#f3f6f5",
            colorText: "#15262d",
            colorTextSecondary: "#66777c",
            borderRadius: 9,
            borderRadiusLG: 13,
            fontFamily: 'Inter, ui-sans-serif, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif',
          },
          components: {
            Button: { controlHeight: 38, fontWeight: 600 },
            Drawer: { paddingLG: 24 },
            Form: { itemMarginBottom: 20 },
            Table: { headerBg: "#f7f9f8", headerColor: "#6b7a7e", rowHoverBg: "#f8fbfa" },
          },
        }}
      >
        <AntApp>
          <ConsoleApp />
        </AntApp>
      </ConfigProvider>
  );
}

createRoot(root).render(
  <StrictMode>
    <QueryClientProvider client={queryClient}>
      <LocalizedConsole />
    </QueryClientProvider>
  </StrictMode>,
);
