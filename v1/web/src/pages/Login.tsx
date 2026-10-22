import * as React from 'react';
import { Navigate, useLocation, useNavigate } from 'react-router-dom';
import { Boxes } from 'lucide-react';
import { useMe, useLogin } from '@/lib/queries';
import { ApiError } from '@/lib/api';
import { Button } from '@/components/ui/button';
import { Field, Input } from '@/components/ui/form';
import { Note } from '@/components/app/primitives';

export function LoginPage() {
  const me = useMe();
  const login = useLogin();
  const nav = useNavigate();
  const loc = useLocation();
  const [u, setU] = React.useState('');
  const [p, setP] = React.useState('');
  // 服务端回 totp_required（密码已对）后才出现验证码框；密码保留着，填上码再提交一次
  const [needCode, setNeedCode] = React.useState(false);
  const [code, setCode] = React.useState('');
  const codeRef = React.useRef<HTMLInputElement>(null);
  React.useEffect(() => { if (needCode) codeRef.current?.focus(); }, [needCode]);
  const from = (loc.state as { from?: string } | null)?.from ?? '/';
  // 只信“当前确实登录着”的 me：会话过期后 me 带着旧数据又报错时，不能跳回去（否则和外壳互相重定向）
  if (me.data && !me.isError) return <Navigate to={from} replace />;
  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    login.mutate({ username: u.trim(), password: p, ...(needCode ? { totp: code.trim() } : {}) }, {
      onSuccess: () => nav(from, { replace: true }),
      onError: (err) => {
        if (err instanceof ApiError && err.code === 'totp_required') setNeedCode(true);
        if (err instanceof ApiError && err.code === 'totp_invalid') { setCode(''); codeRef.current?.focus(); }
      },
    });
  };
  const err = login.error instanceof ApiError && login.error.code === 'totp_required' ? null : (login.error as Error | null);
  return (
    <div className="login-screen">
      <form className="login-card" onSubmit={submit}>
        <div className="login-brand"><span className="brand-mark"><Boxes size={14} /></span>团队线路管理</div>
        <div><h1 className="dialog-title">登录</h1></div>
        {err && <Note tone="bad">{err.message}</Note>}
        <Field label="用户名"><Input value={u} onChange={(e) => { setU(e.target.value); setNeedCode(false); setCode(''); }} autoComplete="username" autoFocus /></Field>
        <Field label="密码"><Input type="password" value={p} onChange={(e) => setP(e.target.value)} autoComplete="current-password" /></Field>
        {needCode && (
          <Field label="两步验证码" info="手机丢了时，输入保存的恢复码（形如 abcd-efgh-jkmn），每个只能用一次">
            <Input ref={codeRef} value={code} onChange={(e) => setCode(e.target.value)} autoComplete="one-time-code" spellCheck={false} placeholder="6 位验证码或恢复码" maxLength={20} className="mono" />
          </Field>
        )}
        <Button variant="primary" size="lg" className="btn-block" type="submit" loading={login.isPending} disabled={!u.trim() || !p || (needCode && !code.trim())}>登录</Button>
      </form>
    </div>
  );
}
