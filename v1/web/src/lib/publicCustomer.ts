import { useQuery } from '@tanstack/react-query';
import { get } from './api';
import type { PublicCustomer } from './types';

// 单独成文件：客户页只要它，不必带上 queries.ts 里管理端的全部查询
/** 公开页：不登录；令牌不对或已停用 404（不区分） */
export const usePublicCustomer = (token: string) => useQuery({
  queryKey: ['public-customer', token], queryFn: ({ signal }) => get<PublicCustomer>(`/public/customer/${encodeURIComponent(token)}`, signal), staleTime: 60_000,
});
