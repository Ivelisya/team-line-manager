import { useQuery } from '@tanstack/react-query';
import { api } from './api';
import type { PublicCustomer } from './types';

export const usePublicCustomer = (token: string, month: string, access: { id: number; password: string } | null) => useQuery<PublicCustomer>({
  queryKey: ['public-customer', token, month, access?.id],
  placeholderData: (previous, query) => query?.queryKey[1] === token && query.queryKey[3] === access?.id ? previous : undefined,
  enabled: !!access, gcTime: 0, staleTime: 0, retry: false,
  queryFn: ({ signal }) => api<PublicCustomer>(`/public/customer/${encodeURIComponent(token)}${month ? `?month=${month}` : ''}`, {
    method: 'POST', body: { password: access!.password }, signal,
  }),
});
