// 设置 · 账户安全：两步验证（开启 / 停用 / 重新生成恢复码，恢复码只显示一次）、修改密码、登录会话（撤销、退出其它设备）。
// 输错当前密码 / 验证码时服务端回 401（code 不是 unauthorized，不会跳登录页），错误留在对话框或表单里显示。
import * as React from 'react';
import { Copy, LogOut, MonitorSmartphone, RefreshCw, ShieldAlert, ShieldCheck, ShieldOff } from 'lucide-react';
import { useChangePassword, useMe, useRegenRecovery, useRevokeOtherSessions, useRevokeSession, useSessions, useTotpDisable, useTotpEnable, useTotpSetup, useTotpStatus } from '@/lib/queries';
import type { AccountSession } from '@/lib/types';
import * as F from '@/lib/format';
import { Button, IconButton } from '@/components/ui/button';
import { Field, Input } from '@/components/ui/form';
import { Trunc } from '@/components/ui/overlays';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { SkeletonRows } from '@/components/ui/skeleton';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog, FormDialog } from '@/components/app/dialogs';
import { Chip, CopyButton, Empty, ErrorNote, Note, Panel, Pill } from '@/components/app/primitives';
import { copyText } from '@/lib/clipboard';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import { ShowAll } from '@/components/app/Pagination';

const MIN_PASSWORD = 12;
const msg = (e: unknown) => (e instanceof Error ? e.message : String(e));
const isTotpCode = (s: string) => /^\d{6}$/.test(s.replace(/\s/g, ''));

export function AccountTab() {
  return (
    <>
      <TotpPanel />
      <PasswordPanel />
      <SessionsPanel />
    </>
  );
}

/** 验证码输入框：认证器的 6 位数字，也接受恢复码（字母、连字符）。接受恢复码时不用数字键盘：iOS 的数字键盘切不出字母 */
const CodeInput = React.forwardRef<HTMLInputElement, { value: string; onChange: (v: string) => void; recovery?: boolean; autoFocus?: boolean; id?: string }>(
  ({ value, onChange, recovery, autoFocus, id }, ref) => (
    <Input ref={ref} id={id} value={value} onChange={(e) => onChange(e.target.value)} inputMode={recovery ? undefined : 'numeric'} autoComplete="one-time-code" spellCheck={false} autoFocus={autoFocus}
      className="mono" maxLength={recovery ? 20 : 7} placeholder={recovery ? '6 位验证码或恢复码' : '6 位验证码'} data-1p-ignore data-lpignore="true" />
  ),
);
CodeInput.displayName = 'CodeInput';

/* ================================================================ 两步验证 */
function TotpPanel() {
  const q = useTotpStatus();
  const [dlg, setDlg] = React.useState<'enable' | 'disable' | 'regen' | null>(null);
  const [codes, setCodes] = React.useState<string[] | null>(null); // 新生成的恢复码，只在这个对话框里出现一次
  const s = q.data;
  const close = () => setDlg(null);
  return (
    <>
      <Panel title="两步验证" sub="登录时除了密码，还要输入手机认证器 App 上的 6 位验证码"
        tools={s && (s.enabled
          ? <><Button onClick={() => setDlg('regen')}><RefreshCw />重新生成恢复码</Button><Button onClick={() => setDlg('disable')}><ShieldOff />停用</Button></>
          : <Button variant="primary" onClick={() => setDlg('enable')}><ShieldCheck />开启两步验证</Button>)}>
        {q.isError ? <ErrorNote error={q.error} onRetry={() => void q.refetch()} /> : !s ? <SkeletonRows rows={1} one /> : s.enabled ? (
          <div style={{ display: 'grid', gap: 'var(--sp-12)' }}>
            <div className="row" style={{ gap: 10 }}>
              <Pill tone="ok">已开启</Pill>
              <span className="t-sub">剩余恢复码 <b className="num">{s.recovery_codes_left}</b> 个，每个只能用一次</span>
            </div>
            {s.recovery_codes_left <= 3 && <Note tone="warn" icon={<ShieldAlert size={14} />}>恢复码{s.recovery_codes_left ? `只剩 ${s.recovery_codes_left} 个` : '已经用完'}：手机丢了时可能登不进来，建议重新生成一组并妥善保存。</Note>}
          </div>
        ) : (
          <Note tone="warn" icon={<ShieldAlert size={14} />}>还没有开启。这个账户能以 root 登录所有机器，密码一旦泄露后果严重，建议开启两步验证。</Note>
        )}
      </Panel>
      <EnableTotpDialog open={dlg === 'enable'} onClose={close} onDone={setCodes} />
      <DisableTotpDialog open={dlg === 'disable'} onClose={close} />
      <RegenDialog open={dlg === 'regen'} onClose={close} onDone={setCodes} />
      <RecoveryCodesDialog codes={codes} onClose={() => setCodes(null)} />
    </>
  );
}

/** 开启：打开时生成一把待确认的密钥（10 分钟内有效），扫码后输入验证码确认。每次打开重新挂载、重新生成 */
const EnableTotpDialog = resetOnOpen(function EnableTotpDialog({ open, onClose, onDone }: { open: boolean; onClose: () => void; onDone: (codes: string[]) => void }) {
  const setup = useTotpSetup(); const enable = useTotpEnable();
  const [code, setCode] = React.useState('');
  const [err, setErr] = React.useState<string | null>(null);
  // 只生成一次（开发模式下 effect 会跑两遍：两次请求的密钥不同，界面和服务端可能对不上）
  const started = React.useRef(false);
  const { mutate: start } = setup;
  React.useEffect(() => { if (open && !started.current) { started.current = true; start(); } }, [open, start]);
  const d = setup.data;
  const submit = () => {
    setErr(null);
    enable.mutate(code.replace(/\s/g, ''), {
      onSuccess: (r) => { onClose(); onDone(r.recovery_codes); toast.ok('两步验证已开启'); },
      onError: (e) => { setErr(msg(e)); setCode(''); },
    });
  };
  return (
    <FormDialog size="md" open={open} onOpenChange={(o) => !o && onClose()} title="开启两步验证" desc="开启后，登录时除了密码还要输入认证器上的验证码。"
      submitLabel="验证并开启" onSubmit={submit} busy={enable.isPending} valid={!!d && isTotpCode(code)} dirty={!!setup.data || !!code}>
      {setup.isError ? <ErrorNote error={setup.error} onRetry={() => setup.mutate()} /> : (
        <div className="totp-setup">
          {d ? <img className="totp-qr" src={`data:image/svg+xml;utf8,${encodeURIComponent(d.qr_svg)}`} alt="两步验证二维码" width={168} height={168} />
            : <span className="totp-qr skel" role="status" aria-label="正在生成" />}
          <div className="totp-steps">
            <div><b>1. 扫码</b>：用认证器 App（Google Authenticator、Microsoft Authenticator、1Password 等）扫描左侧二维码。</div>
            <div>扫不了时，在 App 里手动添加，密钥：</div>
            <div className="token-box"><code>{d ? d.secret.replace(/(.{4})/g, '$1 ').trim() : '…'}</code>{d && <CopyButton text={d.secret} label="密钥" />}</div>
            <div><b>2. 确认</b>：输入 App 上显示的 6 位验证码。</div>
          </div>
        </div>
      )}
      <Field label="验证码"><CodeInput value={code} onChange={setCode} autoFocus /></Field>
      {err && <Note tone="bad">{err}</Note>}
    </FormDialog>
  );
});

/** 停用：当前密码 + 验证码（或恢复码） */
const DisableTotpDialog = resetOnOpen(function DisableTotpDialog({ open, onClose }: { open: boolean; onClose: () => void }) {
  const disable = useTotpDisable(); const me = useMe();
  const [pw, setPw] = React.useState(''); const [code, setCode] = React.useState('');
  const [err, setErr] = React.useState<string | null>(null);
  const submit = () => {
    setErr(null);
    disable.mutate({ password: pw, code: code.trim() }, { onSuccess: () => { onClose(); toast.ok('两步验证已停用'); }, onError: (e) => setErr(msg(e)) });
  };
  return (
    <FormDialog size="sm" open={open} onOpenChange={(o) => !o && onClose()} title="停用两步验证？" desc="停用后只凭密码就能登录，现有的恢复码全部作废。"
      submitLabel="停用" onSubmit={submit} busy={disable.isPending} valid={!!pw && !!code.trim()} dirty={!!pw || !!code}>
      <input type="text" autoComplete="username" value={me.data?.username ?? ''} readOnly hidden />
      <Field label="当前密码"><Input type="password" value={pw} onChange={(e) => setPw(e.target.value)} autoComplete="current-password" autoFocus /></Field>
      <Field label="两步验证码" hint="手机不在身边时可以用一个恢复码"><CodeInput value={code} onChange={setCode} recovery /></Field>
      {err && <Note tone="bad">{err}</Note>}
    </FormDialog>
  );
});

/** 重新生成恢复码：旧的全部作废 */
const RegenDialog = resetOnOpen(function RegenDialog({ open, onClose, onDone }: { open: boolean; onClose: () => void; onDone: (codes: string[]) => void }) {
  const regen = useRegenRecovery();
  const [code, setCode] = React.useState('');
  const [err, setErr] = React.useState<string | null>(null);
  const submit = () => {
    setErr(null);
    regen.mutate(code.trim(), { onSuccess: (r) => { onClose(); onDone(r.recovery_codes); }, onError: (e) => { setErr(msg(e)); setCode(''); } });
  };
  return (
    <FormDialog size="sm" open={open} onOpenChange={(o) => !o && onClose()} title="重新生成恢复码？" desc="生成一组新的 10 个恢复码，旧的（包括没用过的）立即作废。"
      submitLabel="重新生成" onSubmit={submit} busy={regen.isPending} valid={!!code.trim()} dirty={!!code}>
      <Field label="两步验证码"><CodeInput value={code} onChange={setCode} recovery autoFocus /></Field>
      {err && <Note tone="bad">{err}</Note>}
    </FormDialog>
  );
});

/** 新恢复码：只显示这一次，点「我已保存」才关（点遮罩、Esc 不关） */
function RecoveryCodesDialog({ codes, onClose }: { codes: string[] | null; onClose: () => void }) {
  const held = useHeld(codes ? 'codes' : null, codes) ?? [];
  const copyRef = React.useRef<HTMLButtonElement>(null);
  return (
    <Dialog open={codes != null} onOpenChange={(o) => !o && onClose()}>
      <DialogContent size="md" onInteractOutside={(e) => e.preventDefault()} onEscapeKeyDown={(e) => e.preventDefault()} onOpenAutoFocus={(e) => { e.preventDefault(); copyRef.current?.focus(); }}>
        <DialogHeader><DialogTitle>保存恢复码</DialogTitle><DialogDescription>手机丢了或认证器不能用时，在登录页的验证码框里输入一个恢复码代替验证码。</DialogDescription></DialogHeader>
        <Note tone="warn" icon={<ShieldAlert size={14} />}><b>只显示这一次。</b>每个只能用一次。请存进密码管理器或打印出来，和手机分开保管。</Note>
        <div className="recovery-grid">{held.map((c) => <code key={c}>{c}</code>)}</div>
        <DialogFooter>
          <Button ref={copyRef} onClick={() => void copyText(held.join('\n'), '恢复码')}><Copy />复制全部</Button>
          <Button variant="primary" onClick={onClose}>我已保存</Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/* ================================================================ 修改密码 */
function PasswordPanel() {
  const me = useMe(); const totp = useTotpStatus().data?.enabled ?? false;
  const change = useChangePassword();
  const [cur, setCur] = React.useState(''); const [nw, setNw] = React.useState(''); const [nw2, setNw2] = React.useState(''); const [code, setCode] = React.useState('');
  const [err, setErr] = React.useState<string | null>(null);
  const len = [...nw].length;
  const sameAsUser = !!me.data && nw.trim().toLowerCase() === me.data.username.trim().toLowerCase();
  const newErr = nw && len < MIN_PASSWORD ? `至少 ${MIN_PASSWORD} 个字符` : sameAsUser ? '不能和用户名一样' : nw && nw === cur ? '和当前密码一样' : undefined;
  const valid = !!cur && len >= MIN_PASSWORD && !newErr && nw === nw2 && (!totp || !!code.trim());
  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    if (!valid || change.isPending) return;
    setErr(null);
    change.mutate({ current: cur, new: nw, ...(totp ? { totp: code.trim() } : {}) }, {
      onSuccess: (r) => {
        setCur(''); setNw(''); setNw2(''); setCode('');
        toast.ok(r.revoked_sessions ? `密码已修改，其它 ${r.revoked_sessions} 个设备已退出登录` : '密码已修改');
      },
      onError: (x) => { setErr(msg(x)); setCode(''); },
    });
  };
  return (
    <Panel title="修改密码" sub="改完后，除本设备以外的登录会话全部退出">
      <form className="account-form" onSubmit={submit}>
        <input type="text" autoComplete="username" value={me.data?.username ?? ''} readOnly hidden />
        <div className="form-grid">
          <Field label="当前密码"><Input type="password" value={cur} onChange={(e) => setCur(e.target.value)} autoComplete="current-password" /></Field>
          {totp && <Field label="两步验证码" hint="或一个恢复码"><CodeInput value={code} onChange={setCode} recovery /></Field>}
        </div>
        <div className="form-grid">
          <Field label="新密码" error={newErr} hint={`至少 ${MIN_PASSWORD} 个字符，建议用密码管理器生成`}><Input type="password" value={nw} onChange={(e) => setNw(e.target.value)} autoComplete="new-password" /></Field>
          <Field label="再输一次新密码" error={nw2 && nw !== nw2 ? '两次输入的不一样' : undefined}><Input type="password" value={nw2} onChange={(e) => setNw2(e.target.value)} autoComplete="new-password" /></Field>
        </div>
        {err && <Note tone="bad">{err}</Note>}
        <div className="row"><Button type="submit" variant="primary" loading={change.isPending} disabled={!valid}>修改密码</Button></div>
      </form>
    </Panel>
  );
}

/* ================================================================ 登录会话 */
/** User-Agent 的简短说法：浏览器 · 系统；认不出的显示原文第一段 */
function uaLabel(ua: string | null): string {
  if (!ua) return '未知设备';
  const b = /Edg\//.test(ua) ? 'Edge' : /Firefox\//.test(ua) ? 'Firefox' : /Chrome\//.test(ua) ? 'Chrome' : /Safari\//.test(ua) ? 'Safari' : null;
  const os = /iPhone|iPad/.test(ua) ? 'iOS' : /Android/.test(ua) ? 'Android' : /Windows/.test(ua) ? 'Windows' : /Mac OS X|Macintosh/.test(ua) ? 'macOS' : /Linux/.test(ua) ? 'Linux' : null;
  return b ? [b, os].filter(Boolean).join(' · ') : ua.split(' ')[0];
}

const SESSION_CAP = 8;
function SessionsPanel() {
  const q = useSessions(); const revoke = useRevokeSession(); const others = useRevokeOtherSessions();
  const [ask, setAsk] = React.useState<AccountSession | 'others' | null>(null);
  const askHeld = useHeld(ask === 'others' ? 'others' : ask?.id, ask);
  const list = q.data ?? [];
  const nOthers = list.filter((s) => !s.current).length;
  // 会话可能有一二十个（多是开启设备记录之前的「未知设备」）：先露前 SESSION_CAP 个（按服务端的顺序，最近活动的在前），表尾展开全部
  const [all, setAll] = React.useState(false);
  const shown = all ? list : list.slice(0, SESSION_CAP);
  return (
    <>
      <Panel title="登录会话" count={list.length} sub="正在登录着的设备；不认识的立即撤销" flush
        tools={<Button disabled={nOthers === 0} onClick={() => setAsk('others')}><LogOut />退出其它设备</Button>}>
        {q.isError && <div style={{ padding: 'var(--sp-12) var(--card-pad)' }}><ErrorNote error={q.error} onRetry={() => void q.refetch()} /></div>}
        {q.isLoading ? <SkeletonRows rows={2} head /> : q.isError && !q.data ? null : list.length === 0 ? <Empty icon={<MonitorSmartphone size={22} />} text="没有登录会话" /> : (
          <div className="table-wrap">
            <table className="dt" style={{ minWidth: 780 }}>
              <colgroup><col style={{ width: 260 }} /><col style={{ width: 170 }} /><col style={{ width: 112 }} /><col style={{ width: 128 }} /><col style={{ width: 112 }} /><col style={{ width: 52 }} /></colgroup>
              <thead><tr><th>设备</th><th>IP</th><th>登录</th><th>最近活动</th><th>到期</th><th /></tr></thead>
              <tbody>
                {shown.map((s) => (
                  <tr key={s.id}>
                    <td>
                      <div className="row" style={{ flexWrap: 'nowrap', gap: 8 }}><Trunc className="cell-main strong">{uaLabel(s.user_agent)}</Trunc>{s.current && <Chip>本设备</Chip>}</div>
                      <span className="sub"><Trunc text={s.user_agent ?? ''}>{s.user_agent ?? '没有记录 User-Agent'}</Trunc></span>
                    </td>
                    <td>{s.ip ? <Trunc className="mono">{s.ip}</Trunc> : <span className="t-sub">未知</span>}</td>
                    <td>{F.dt(s.created_at)}</td>
                    <td>{F.dt(s.last_seen_at)}<span className="sub">{F.ago(s.last_seen_at)}</span></td>
                    <td>{F.dt(s.expires_at)}</td>
                    <td className="c">{!s.current && <IconButton label="撤销会话" size="sm" onClick={() => setAsk(s)}><LogOut size={14} /></IconButton>}</td>
                  </tr>
                ))}
              </tbody>
            </table>
            <ShowAll total={list.length} cap={SESSION_CAP} open={all} onToggle={() => setAll((v) => !v)} unit="个" />
          </div>
        )}
      </Panel>
      <ConfirmDialog open={!!ask} onOpenChange={(o) => !o && setAsk(null)} confirmLabel={askHeld === 'others' ? '全部退出' : '撤销'} danger
        title={askHeld === 'others' ? `退出其它 ${nOthers} 个设备？` : `撤销「${askHeld ? uaLabel(askHeld.user_agent) : ''}」的会话？`}
        description={askHeld === 'others' ? '除本设备外的登录会话立即失效，那些设备要重新登录。' : `${askHeld?.ip ? `来自 ${askHeld.ip} 的` : '这个'}会话立即失效，那台设备要重新登录。`}
        onConfirm={async () => {
          if (ask === 'others') { const r = await others.mutateAsync(); toast.ok(`已退出 ${r.revoked} 个设备`); }
          else if (ask) { await revoke.mutateAsync(ask.id); toast.ok('会话已撤销'); }
        }} />
    </>
  );
}
