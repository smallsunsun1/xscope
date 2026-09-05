import { useEffect, useRef, useSyncExternalStore } from "react";
import type { FormInstance } from "antd";
import { messages } from "./locales/messages";

import { getLocale, setLocale, subscribeLocale } from "./locale.ts";
export { getLocale, setLocale } from "./locale.ts";
export type { Locale } from "./locale.ts";
export type MessageKey = keyof typeof messages;

/** Chinese source keys are checked against the English catalog by TypeScript. */
export function t(key: MessageKey, values: Record<string, string | number> = {}): string {
  const template = getLocale() === "zh-CN" ? key : messages[key];
  return template.replace(/\{(\w+)\}/g, (placeholder, name: string) => String(values[name] ?? placeholder));
}

export function useI18n() {
  const current = useSyncExternalStore(subscribeLocale, getLocale);
  return { locale: current, setLocale, t };
}

/** Revalidate only visible errors so a language change does not reset a draft. */
export function useLocalizedForm(form: FormInstance) {
  const { locale: current } = useI18n();
  const previous = useRef(current);
  useEffect(() => {
    if (previous.current === current) return;
    previous.current = current;
    // Ant Design updates field labels and validation messages through context.
    // Let that commit finish before asking its form store to validate again.
    const timer = window.setTimeout(() => {
      const fields = form.getFieldsError().filter(field => field.errors.length).map(field => field.name);
      if (fields.length) void form.validateFields(fields).catch(() => { /* Errors are rendered by Form. */ });
    }, 0);
    return () => window.clearTimeout(timer);
  }, [current, form]);
}
