// 对象已经不存在（接口 404 / not_found）：不是网络出错，重试没用——写明「已不存在」，配一个回去的出口。网络错误、5xx 仍走 ErrorNote 的「重试」
import * as React from 'react';
import { ApiError } from '@/lib/api';
import { Empty } from './primitives';

export const isGone = (e: unknown) => e instanceof ApiError && e.code === 'not_found';

export function Gone({ text, children }: { text: string; children: React.ReactNode }) {
  return <Empty text={text}>{children}</Empty>;
}
