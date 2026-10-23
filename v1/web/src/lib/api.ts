// fetch 封装：成功解包 {data}，失败抛 ApiError（message 是中文）。写请求自动带 X-Fleet-Request。

export class ApiError extends Error {
  constructor(public status: number, public code: string, message: string) { super(message); }
}

type Opts = { method?: string; body?: unknown; signal?: AbortSignal };

/** 401 时通知外壳跳转登录 */
export const AUTH_EVENT = 'fleet:unauthorized';

export async function api<T = unknown>(path: string, opts: Opts = {}): Promise<T> {
  const method = (opts.method ?? 'GET').toUpperCase();
  const headers: Record<string, string> = { Accept: 'application/json' };
  if (method !== 'GET') headers['X-Fleet-Request'] = '1';
  let body: BodyInit | undefined;
  if (opts.body !== undefined) { headers['Content-Type'] = 'application/json'; body = JSON.stringify(opts.body); }
  let res: Response;
  try {
    res = await fetch(`/api/v1${path}`, { method, headers, body, credentials: 'same-origin', signal: opts.signal });
  } catch (e) {
    if ((e as Error).name === 'AbortError') throw e;
    throw new ApiError(0, 'network', '无法连接到中控，请检查网络');
  }
  let json: { data?: T; error?: { code: string; message: string } } | null = null;
  try { json = await res.json(); } catch {
    // 出错的响应可能没有 JSON（网关的 502 页面），下面按状态码报；2xx 却解析不了（被代理改写、截断）不能当成功悄悄返回 undefined
    if (res.ok && res.status !== 204) throw new ApiError(res.status, 'bad_response', '中控返回的内容读不懂');
  }
  if (!res.ok) {
    // 只有「没登录 / 会话失效」才跳登录页；账户安全里输错当前密码、验证码也是 401，但 code 不同（bad_password / totp_invalid…）
    const code = json?.error?.code ?? 'unauthorized';
    if (res.status === 401 && !path.startsWith('/auth/') && code === 'unauthorized') window.dispatchEvent(new Event(AUTH_EVENT));
    throw new ApiError(res.status, json?.error?.code ?? 'http', json?.error?.message ?? `请求失败（${res.status}）`);
  }
  return json?.data as T;
}

export const get = <T>(p: string, signal?: AbortSignal) => api<T>(p, { signal });
export const post = <T = unknown>(p: string, body?: unknown) => api<T>(p, { method: 'POST', body: body ?? {} });
export const put = <T = unknown>(p: string, body?: unknown) => api<T>(p, { method: 'PUT', body });
export const patch = <T = unknown>(p: string, body?: unknown) => api<T>(p, { method: 'PATCH', body });
export const del = <T = unknown>(p: string) => api<T>(p, { method: 'DELETE' });
