// mock 的汇率（同服务端 fx.rs）：自动的值是「1 USD 合多少该币种」，手动覆盖优先，USD 恒为 1；没有汇率的不折算。
// 折算 = round(分 × 1 单位合多少 USD)；合计是逐台折好的相加
import type { FxRates, UsdTotal } from '@/lib/types';

const DAY = 86400_000;
const state = {
  auto: { CNY: 7.1234, EUR: 0.8925, HKD: 7.8398, JPY: 157.05, GBP: 0.7476, SGD: 1.2776 } as Record<string, number>,
  manual: {} as Record<string, { usd_per_unit: number; by: string; at: string }>,
  fetched_at: new Date(Date.now() - 0.3 * DAY).toISOString(),
  rates_date: new Date(Date.now() - DAY).toISOString().slice(0, 10),
};

export function usdPerUnit(currency: string): number | null {
  if (currency === 'USD') return 1;
  const m = state.manual[currency]; if (m) return m.usd_per_unit;
  const a = state.auto[currency];
  return a ? 1 / a : null;
}
export const toUsd = (cents: number, currency: string) => { const r = usdPerUnit(currency); return r == null ? null : Math.round(cents * r); };

/** 月费合计（USD），同总览的 monthly_usd */
export function usdTotal(items: { currency: string; monthly_cents: number }[]): UsdTotal {
  const t: UsdTotal = { cents: 0, by_currency: [], unconverted: [] };
  for (const { currency, monthly_cents } of items) {
    const r = usdPerUnit(currency); const u = toUsd(monthly_cents, currency);
    if (r == null || u == null) {
      const p = t.unconverted.find((x) => x.currency === currency);
      if (p) p.cents += monthly_cents; else t.unconverted.push({ currency, cents: monthly_cents });
      continue;
    }
    t.cents += u;
    const p = t.by_currency.find((x) => x.currency === currency);
    if (p) { p.cents += monthly_cents; p.usd_cents += u; } else t.by_currency.push({ currency, cents: monthly_cents, usd_cents: u, usd_per_unit: r });
  }
  t.by_currency.sort((a, b) => a.currency.localeCompare(b.currency)); t.unconverted.sort((a, b) => a.currency.localeCompare(b.currency));
  return t;
}

type Reg = (method: string, path: string, h: (c: { params: string[]; body: any }) => unknown) => void; // eslint-disable-line @typescript-eslint/no-explicit-any

export function installFxMock(R: Reg, inUse: () => string[], user: () => string, changed: () => void, bad: (msg: string) => Error) {
  const view = (): FxRates => {
    const used = new Set(inUse());
    const all = [...new Set([...used, ...Object.keys(state.auto), ...Object.keys(state.manual), 'USD'])]
      .sort((a, b) => Number(!used.has(a)) - Number(!used.has(b)) || a.localeCompare(b));
    return {
      base: 'USD', auto: true, url: 'https://api.frankfurter.dev/v1/latest?base=USD', fetched_at: state.fetched_at, rates_date: state.rates_date, source: 'https://api.frankfurter.dev/v1/latest?base=USD',
      last_attempt_at: state.fetched_at, last_error: null, missing: [...used].filter((c) => usdPerUnit(c) == null).sort(),
      rates: all.map((c) => {
        const r = usdPerUnit(c);
        return { currency: c, usd_per_unit: r, per_usd: r == null ? null : 1 / r, source: c === 'USD' ? 'base' : state.manual[c] ? 'manual' : r != null ? 'auto' : null,
          auto_per_usd: state.auto[c] ?? null, manual: state.manual[c] ?? null, in_use: used.has(c) };
      }),
    };
  };
  R('GET', '/settings/fx-rates', () => view());
  R('POST', '/settings/fx-rates/refresh', () => { state.fetched_at = new Date().toISOString(); changed(); return view(); });
  R('PUT', '/settings/fx-rates/([^/]+)', ({ params, body }) => {
    const c = decodeURIComponent(params[0]).toUpperCase();
    if (c === 'USD') throw bad('USD 是折算的目标币种，汇率恒为 1，不能设置');
    const v = body.usd_per_unit ?? (body.per_usd ? 1 / body.per_usd : null);
    if (!(v > 0)) throw bad('汇率要是正数');
    state.manual[c] = { usd_per_unit: v, by: user(), at: new Date().toISOString() }; changed(); return view();
  });
  R('DELETE', '/settings/fx-rates/([^/]+)', ({ params }) => { delete state.manual[decodeURIComponent(params[0]).toUpperCase()]; changed(); return view(); });
}
