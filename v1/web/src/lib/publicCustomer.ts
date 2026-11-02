import { keepPreviousData, useQuery } from '@tanstack/react-query';
import { get } from './api';
import type { PublicCustomer } from './types';

// 单独成文件：客户页只要它，不必带上 queries.ts 里管理端的全部查询
/** 公开页：不登录；month = YYYY-MM，空 = 本月；令牌不对或已停用 404（不区分）。翻月时先留着上一个月的（调淡），不塌成骨架 */
export const usePublicCustomer = (token: string, month: string) => useQuery({
  queryKey: ['public-customer', token, month], staleTime: 60_000, placeholderData: keepPreviousData,
  queryFn: ({ signal }) => get<PublicCustomer>(`/public/customer/${encodeURIComponent(token)}${month ? `?month=${month}` : ''}`, signal),
});
