export function formatMoney(amount: number, currency: string): string {
  const formatter = new Intl.NumberFormat("zh-CN", {
    style: "currency",
    currency,
    maximumFractionDigits: 6,
  });
  const fractionDigits = formatter.resolvedOptions().minimumFractionDigits ?? 2;
  return formatter.format(amount / 10 ** fractionDigits);
}

export function formatNumber(value: number): string {
  return new Intl.NumberFormat("zh-CN").format(value);
}

export function formatDate(value: string | undefined): string {
  if (!value) return "—";
  return new Intl.DateTimeFormat("zh-CN", {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  }).format(new Date(value));
}
