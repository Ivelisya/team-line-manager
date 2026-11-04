import { useEffect, useState } from 'react';
import { useQuery } from '@tanstack/react-query';
import { api } from './api';
import type { PublicCustomer } from './types';

export function useExpired(expiresAt?: string) {
  const deadline = expiresAt ? Date.parse(expiresAt) : Infinity;
  const [, refresh] = useState(0);
  useEffect(() => {
    if (!Number.isFinite(deadline)) return;
    const check = () => refresh(n => n + 1);
    const timer = window.setTimeout(check, Math.max(0, deadline - Date.now()));
    window.addEventListener('focus', check);
    return () => { clearTimeout(timer); window.removeEventListener('focus', check); };
  }, [deadline]);
  return Date.now() >= deadline;
}

export const usePublicCustomer = (token: string, month: string, access: { id: number; password: string } | null) => useQuery<PublicCustomer>({
  queryKey: ['public-customer', token, month, access?.id],
  placeholderData: (previous, query) => query?.queryKey[1] === token && query.queryKey[3] === access?.id ? previous : undefined,
  enabled: !!access, gcTime: 0, staleTime: 0, retry: false,
  queryFn: ({ signal }) => api<PublicCustomer>(`/public/customer/${encodeURIComponent(token)}${month ? `?month=${month}` : ''}`, {
    method: 'POST', body: { password: access!.password }, signal,
  }),
});
