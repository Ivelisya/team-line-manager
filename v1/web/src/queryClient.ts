import { QueryClient } from '@tanstack/react-query';
import { ApiError } from './lib/api';

/** 管理端与客户页共用的查询客户端 */
export const qc = new QueryClient({
  defaultOptions: {
    queries: {
      staleTime: 10_000,
      // 4xx 是确定的结果（未登录、不存在、参数不对），重试没有意义；网络错误和 5xx 重试一次
      retry: (n, err) => !(err instanceof ApiError && err.status >= 400 && err.status < 500) && n < 1,
      refetchOnWindowFocus: true,
    },
  },
});
