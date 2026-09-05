import { getLocale } from "./locale.ts";

export function formatMoney(amount: number, currency: string): string {
  const formatter = new Intl.NumberFormat(getLocale(), {
    style: "currency",
    currency,
    maximumFractionDigits: 6,
  });
  const fractionDigits = formatter.resolvedOptions().minimumFractionDigits ?? 2;
  return formatter.format(amount / 10 ** fractionDigits);
}

export function formatNumber(value: number): string {
  return new Intl.NumberFormat(getLocale()).format(value);
}

// Exact i64 decimal strings: one microunit is 1 / 1,000,000 minor units.
export function formatMicrounits(value: string, currency: string): string {
  const digits = new Intl.NumberFormat(getLocale(), { style: "currency", currency }).resolvedOptions().maximumFractionDigits ?? 2;
  const scale = 10n ** BigInt(digits + 6);
  const amount = BigInt(value);
  const absolute = amount < 0n ? -amount : amount;
  const fraction = (absolute % scale).toString().padStart(digits + 6, "0").replace(/0+$/, "").padEnd(digits, "0");
  return `${currency} ${amount < 0n ? "−" : ""}${(absolute / scale).toLocaleString(getLocale())}${fraction ? `.${fraction}` : ""}`;
}

export function formatDate(value: string | undefined): string {
  if (!value) return "—";
  return new Intl.DateTimeFormat(getLocale(), {
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  }).format(new Date(value));
}
