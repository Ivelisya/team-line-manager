// 实时流：EventSource /api/v1/stream，按事件类型让对应查询失效。断线由浏览器自动重连；另有 30s 轮询兜底。
import { useEffect, useState } from 'react';
import { useQueryClient } from '@tanstack/react-query';
import { invalidateFor } from './queries';

export function useStream(enabled: boolean) {
  const qc = useQueryClient();
  const [state, setState] = useState<'connecting' | 'open' | 'error'>('connecting');
  useEffect(() => {
    if (!enabled) return;
    const es = new EventSource('/api/v1/stream');
    es.onopen = () => setState('open');
    es.onerror = () => setState('error');
    es.onmessage = (m) => {
      setState('open');
      try { invalidateFor(qc, JSON.parse(m.data)); } catch { /* 忽略非 JSON 心跳 */ }
    };
    return () => es.close();
  }, [enabled, qc]);
  return state;
}
