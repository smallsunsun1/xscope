// Framework-free locale state, also usable by Node-based formatting checks.
export type Locale = "zh-CN" | "en-US";
const storageKey = "xscope.locale";
const listeners = new Set<() => void>();

function initialLocale(): Locale {
  if (typeof window === "undefined") return "zh-CN";
  try {
    const saved = localStorage.getItem(storageKey);
    if (saved === "zh-CN" || saved === "en-US") return saved;
  } catch { /* Storage can be disabled; keep an in-memory preference. */ }
  return navigator.language.toLowerCase().startsWith("zh") ? "zh-CN" : "en-US";
}

let locale = initialLocale();
export const getLocale = () => locale;

function updateLocale(next: Locale) {
  locale = next;
  if (typeof document !== "undefined") {
    document.documentElement.lang = next;
    document.title = next === "zh-CN" ? "XScope · 模型平台控制台" : "XScope · Model Platform Console";
  }
  listeners.forEach(listener => listener());
}

export function setLocale(next: Locale) {
  try { localStorage.setItem(storageKey, next); } catch { /* Switching still works without persistence. */ }
  updateLocale(next);
}

if (typeof window !== "undefined") {
  window.addEventListener("storage", event => {
    if (event.storageArea === localStorage && (event.key === storageKey || event.key === null)) updateLocale(initialLocale());
  });
  updateLocale(locale);
}

export const subscribeLocale = (listener: () => void) => {
  listeners.add(listener);
  return () => { listeners.delete(listener); };
};
