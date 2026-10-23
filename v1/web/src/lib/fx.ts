// 汇率（计费统一按 USD 展示，见 docs/API.md「汇率」）：设置页的查询与修改，以及录入价格时「折合多少美元」的预览。
// 展示用的折算值（月费、合计）都由服务端算好放在视图里，这里不算；只有录入框旁的预览按服务端给的汇率、同样的取整（四舍五入到分）估一下。
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query';
import { del, get, post, put } from './api';
import type { FxRates } from './types';
import * as F from './format';

/** 汇率设置；改动（手动、自动更新）由实时流的 settings 事件失效 */
export const useFxRates = () => useQuery({ queryKey: ['fx-rates'], queryFn: ({ signal }) => get<FxRates>('/settings/fx-rates', signal), staleTime: Infinity });

/** 机器视图、总览、团队与套餐里的 USD 字段随汇率变：改完一起重取 */
const AFFECTED = [['machines'], ['machine'], ['overview'], ['teams'], ['plans']];
function useFxWrite<A>(fn: (a: A) => Promise<FxRates>) {
  const qc = useQueryClient();
  return useMutation({
    mutationFn: fn,
    onSuccess: (d) => { qc.setQueryData(['fx-rates'], d); for (const k of AFFECTED) void qc.invalidateQueries({ queryKey: k }); },
  });
}
/** 手动设置一个币种的汇率：给 usd_per_unit（1 单位合多少 USD）或 per_usd（1 USD 合多少该币种） */
export const useSetFxRate = () => useFxWrite(({ currency, ...b }: { currency: string; usd_per_unit?: number; per_usd?: number }) => put<FxRates>(`/settings/fx-rates/${encodeURIComponent(currency)}`, b));
/** 取消手动汇率，回到自动更新的值 */
export const useClearFxRate = () => useFxWrite((currency: string) => del<FxRates>(`/settings/fx-rates/${encodeURIComponent(currency)}`));
/** 立即从汇率源更新（失败时中控回 502，保留上次的值） */
export const useRefreshFx = () => useFxWrite(() => post<FxRates>('/settings/fx-rates/refresh'));

/** 录入价格时的预览：「≈ $42.13」；USD 本身、空值不提示；没有汇率时说明不折算 */
export function usdPreview(rates: FxRates | undefined, cents: number | null, currency: string): string | null {
  if (cents == null || !isFinite(cents) || !rates || currency === 'USD') return null;
  const r = rates.rates.find((x) => x.currency === currency)?.usd_per_unit;
  return r != null ? `≈ ${F.money(Math.round(cents * r), 'USD')}（${F.fxRate(currency, r)}）` : `没有 ${currency} 的汇率，费用不折算成 USD（设置 → 汇率）`;
}
/** 同 usdPreview，自己取汇率 */
export function useUsdPreview(cents: number | null, currency: string) { return usdPreview(useFxRates().data, cents, currency); }
