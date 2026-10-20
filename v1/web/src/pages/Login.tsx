import * as React from 'react';
import { Navigate, useLocation, useNavigate } from 'react-router-dom';
import { Boxes } from 'lucide-react';
import { useLogin, useMe } from '@/lib/queries';
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
  const from = (loc.state as { from?: string } | null)?.from ?? '/';
  if (me.data) return <Navigate to={from} replace />;
  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    login.mutate({ username: u.trim(), password: p }, { onSuccess: () => nav(from, { replace: true }) });
  };
  return (
    <div className="login-screen">
      <form className="login-card" onSubmit={submit}>
        <div className="login-brand"><span className="brand-mark"><Boxes size={14} /></span>团队线路管理</div>
        <div><h1 className="dialog-title">登录</h1><p className="dialog-desc" style={{ margin: '4px 0 0' }}>使用中控账号登录。</p></div>
        {login.isError && <Note tone="bad">{(login.error as Error).message}</Note>}
        <Field label="用户名"><Input value={u} onChange={(e) => setU(e.target.value)} autoComplete="username" autoFocus /></Field>
        <Field label="密码"><Input type="password" value={p} onChange={(e) => setP(e.target.value)} autoComplete="current-password" /></Field>
        <Button variant="primary" size="lg" className="btn-block" type="submit" loading={login.isPending} disabled={!u.trim() || !p}>登录</Button>
      </form>
    </div>
  );
}
