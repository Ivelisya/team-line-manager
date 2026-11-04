// 设置 · 账户安全：两步验证（开启 / 停用 / 重新生成恢复码，恢复码只显示一次）、修改密码（可同时撤销全部 API 密钥）、登录会话（撤销、退出其他设备）。
// 开启两步验证、退出其他设备都要再输当前密码：拿到会话 Cookie 的人不能借它绑自己的认证器、把本人踢下线。
// 输错当前密码 / 验证码时服务端回 401（code 不是 unauthorized，不会跳登录页），错误留在对话框或表单里显示。
import * as React from 'react';
import { LogOut, MonitorSmartphone, RefreshCw, ShieldAlert, ShieldCheck, ShieldOff } from 'lucide-react';
import { useChangePassword, useMe, useRegenRecovery, useRevokeOtherSessions, useRevokeSession, useSessions, useTotpDisable, useTotpEnable, useTotpSetup, useTotpStatus } from '@/lib/queries';
import type { AccountSession } from '@/lib/types';
import * as F from '@/lib/format';
import { Button, IconButton } from '@/components/ui/button';
import { CheckRow, FormField } from '@/components/ui/form';
import { InfoTip } from '@/components/ui/info-tip';
import { Input } from '@/components/ui/input';
import { Trunc } from '@/components/ui/tooltip';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { SkeletonRows } from '@/components/app/SkeletonRows';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog, FormDialog } from '@/components/app/dialogs';
import { Chip, CopyButton, CopyIcon, Empty, ErrorNote, Note, Panel, Pill, useCopied } from '@/components/app/primitives';
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
      <Panel title="两步验证" sub={<InfoTip text="登录时除了密码，还要输入手机认证器 App 上的 6 位验证码" />}
        tools={s && (s.enabled
          ? <><Button variant="outline" size="sm" onClick={() => setDlg('regen')}><RefreshCw />重新生成恢复码</Button><Button variant="outline" size="sm" onClick={() => setDlg('disable')}><ShieldOff />停用</Button></>
          : <Button size="sm" onClick={() => setDlg('enable')}><ShieldCheck />开启两步验证</Button>)}>
        {q.isError ? <ErrorNote error={q.error} onRetry={() => void q.refetch()} /> : !s ? <SkeletonRows rows={1} one /> : s.enabled ? (
          <div style={{ display: 'grid', gap: 'var(--sp-12)' }}>
            <div className="row" style={{ gap: 10 }}>
              <Pill tone="ok">已开启</Pill>
              <span className="t-sub">剩余恢复码 <b className="num">{s.recovery_codes_left}</b> 个</span>
            </div>
            {s.recovery_codes_left <= 3 && <Note tone="warn" icon={<ShieldAlert size={14} />}>恢复码{s.recovery_codes_left ? `只剩 ${s.recovery_codes_left} 个` : '已经用完'}，建议重新生成 <InfoTip text="手机丢了时可能登不进来；重新生成一组并妥善保存。" /></Note>}
          </div>
        ) : (
          <Note tone="warn" icon={<ShieldAlert size={14} />}>还没有开启，建议开启 <InfoTip text="这个账户能以 root 登录所有机器，密码一旦泄露后果严重。" /></Note>
        )}
      </Panel>
      <EnableTotpDialog open={dlg === 'enable'} onClose={close} onDone={setCodes} />
      <DisableTotpDialog open={dlg === 'disable'} onClose={close} />
      <RegenDialog open={dlg === 'regen'} onClose={close} onDone={setCodes} />
      <RecoveryCodesDialog codes={codes} onClose={() => setCodes(null)} />
    </>
  );
}

/** 开启：先核对当前密码、生成一把待确认的密钥（10 分钟内有效），扫码后输入验证码确认。每次打开重新挂载、重新生成 */
const EnableTotpDialog = resetOnOpen(function EnableTotpDialog({ open, onClose, onDone }: { open: boolean; onClose: () => void; onDone: (codes: string[]) => void }) {
  const setup = useTotpSetup(); const enable = useTotpEnable(); const me = useMe();
  const [pw, setPw] = React.useState('');
  const [code, setCode] = React.useState('');
  const [err, setErr] = React.useState<string | null>(null);
  const d = setup.data;
  const submit = () => {
    setErr(null);
    if (!d) {
      setup.mutate(pw, { onSuccess: () => setPw(''), onError: (e) => { setErr(msg(e)); setPw(''); } });
      return;
    }
    enable.mutate(code.replace(/\s/g, ''), {
      onSuccess: (r) => { onClose(); onDone(r.recovery_codes); toast.ok('两步验证已开启'); },
      onError: (e) => { setErr(msg(e)); setCode(''); },
    });
  };
  return (
    <FormDialog size="md" open={open} onOpenChange={(o) => !o && onClose()} title="开启两步验证"
      submitLabel={d ? '验证并开启' : '下一步'} onSubmit={submit} busy={setup.isPending || enable.isPending} valid={d ? isTotpCode(code) : !!pw} dirty={!!d || !!pw || !!code}>
      {!d ? (
        <>
          <input type="text" autoComplete="username" value={me.data?.username ?? ''} readOnly hidden />
          <FormField label="当前密码"><Input type="password" value={pw} onChange={(e) => setPw(e.target.value)} autoComplete="current-password" autoFocus /></FormField>
        </>
      ) : (
        <>
          <div className="totp-setup">
            <img className="totp-qr" src={`data:image/svg+xml;utf8,${encodeURIComponent(d.qr_svg)}`} alt="两步验证二维码" width={168} height={168} />
            <div className="totp-steps">
              <div><b>扫码或输入密钥</b> <InfoTip text="用认证器 App（如 Google Authenticator、Microsoft Authenticator、1Password）扫左侧二维码，或手动输入下面的密钥；再在下面填它显示的 6 位验证码" /></div>
              <div className="token-box"><code>{d.secret.replace(/(.{4})/g, '$1 ').trim()}</code><CopyButton text={d.secret} label="密钥" /></div>
            </div>
          </div>
          <FormField label="验证码"><CodeInput value={code} onChange={setCode} autoFocus /></FormField>
        </>
      )}
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
    <FormDialog size="sm" open={open} onOpenChange={(o) => !o && onClose()} title="停用两步验证？" desc={<>恢复码会全部作废 <InfoTip text="停用后只凭密码就能登录。" /></>}
      submitLabel="停用" onSubmit={submit} busy={disable.isPending} valid={!!pw && !!code.trim()} dirty={!!pw || !!code}>
      <input type="text" autoComplete="username" value={me.data?.username ?? ''} readOnly hidden />
      <FormField label="当前密码"><Input type="password" value={pw} onChange={(e) => setPw(e.target.value)} autoComplete="current-password" autoFocus /></FormField>
      <FormField label="两步验证码"><CodeInput value={code} onChange={setCode} recovery /></FormField>
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
    <FormDialog size="sm" open={open} onOpenChange={(o) => !o && onClose()} title="重新生成恢复码？" desc="旧的恢复码会立即作废"
      submitLabel="重新生成" onSubmit={submit} busy={regen.isPending} valid={!!code.trim()} dirty={!!code}>
      <FormField label="两步验证码"><CodeInput value={code} onChange={setCode} recovery autoFocus /></FormField>
      {err && <Note tone="bad">{err}</Note>}
    </FormDialog>
  );
});

/** 新恢复码：只显示这一次，点「我已保存」才关（点遮罩、Esc 不关） */
function RecoveryCodesDialog({ codes, onClose }: { codes: string[] | null; onClose: () => void }) {
  const held = useHeld(codes ? 'codes' : null, codes) ?? [];
  const copyRef = React.useRef<HTMLButtonElement>(null);
  const cp = useCopied();
  return (
    <Dialog open={codes != null} onOpenChange={(o) => !o && onClose()}>
      <DialogContent size="md" onInteractOutside={(e) => e.preventDefault()} onEscapeKeyDown={(e) => e.preventDefault()} onOpenAutoFocus={(e) => { e.preventDefault(); copyRef.current?.focus(); }}>
        <DialogHeader><DialogTitle>保存恢复码</DialogTitle><DialogDescription>登录时可代替验证码 <InfoTip text="手机丢了或认证器不能用时，在登录页的验证码框里输入一个恢复码代替验证码。" /></DialogDescription></DialogHeader>
        <Note tone="warn" icon={<ShieldAlert size={14} />}><b>只显示这一次。</b>每个只能用一次 <InfoTip text="请存进密码管理器或打印出来，和手机分开保管。" /></Note>
        <div className="recovery-grid">{held.map((c) => <code key={c}>{c}</code>)}</div>
        <DialogFooter>
          <Button variant="outline" ref={copyRef} onClick={() => void cp.copy(held.join('\n'), '恢复码')}><CopyIcon done={cp.done} />复制全部</Button>
          <Button onClick={onClose}>我已保存</Button>
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
  const [keys, setKeys] = React.useState(false);
  const [err, setErr] = React.useState<string | null>(null);
  const len = [...nw].length;
  const sameAsUser = !!me.data && nw.trim().toLowerCase() === me.data.username.trim().toLowerCase();
  const newErr = nw && len < MIN_PASSWORD ? `至少 ${MIN_PASSWORD} 个字符` : sameAsUser ? '不能和用户名一样' : nw && nw === cur ? '和当前密码一样' : undefined;
  const valid = !!cur && len >= MIN_PASSWORD && !newErr && nw === nw2 && (!totp || !!code.trim());
  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    if (!valid || change.isPending) return;
    setErr(null);
    change.mutate({ current: cur, new: nw, ...(totp ? { totp: code.trim() } : {}), ...(keys ? { revoke_api_keys: true } : {}) }, {
      onSuccess: (r) => {
        setCur(''); setNw(''); setNw2(''); setCode(''); setKeys(false);
        const done = [r.revoked_sessions ? `其他 ${r.revoked_sessions} 个设备已退出登录` : '', r.revoked_api_keys ? `${r.revoked_api_keys} 个 API 密钥已撤销` : ''].filter(Boolean);
        toast.ok(done.length ? `密码已修改，${done.join('，')}` : '密码已修改');
      },
      onError: (x) => { setErr(msg(x)); setCode(''); },
    });
  };
  return (
    <Panel title="修改密码" sub={<InfoTip text="改完后，除本设备以外的登录会话全部退出；API 密钥不受影响，除非勾选一并撤销" />}>
      <form className="account-form" onSubmit={submit}>
        <input type="text" autoComplete="username" value={me.data?.username ?? ''} readOnly hidden />
        <div className="form-grid">
          <FormField label="当前密码"><Input type="password" value={cur} onChange={(e) => setCur(e.target.value)} autoComplete="current-password" /></FormField>
          {totp && <FormField label="两步验证码"><CodeInput value={code} onChange={setCode} recovery /></FormField>}
        </div>
        <div className="form-grid">
          <FormField label="新密码" error={newErr} info="建议用密码管理器生成"><Input type="password" value={nw} onChange={(e) => setNw(e.target.value)} autoComplete="new-password" placeholder={`至少 ${MIN_PASSWORD} 个字符`} /></FormField>
          <FormField label="再输一次新密码" error={nw2 && nw !== nw2 ? '两次输入的不一样' : undefined}><Input type="password" value={nw2} onChange={(e) => setNw2(e.target.value)} autoComplete="new-password" /></FormField>
        </div>
        <div className="row" style={{ gap: 4 }}><CheckRow checked={keys} onChange={setKeys}>同时撤销所有 API 密钥</CheckRow><InfoTip text="怀疑密码或登录会话泄露时勾上：拿到会话的人可能已经建了密钥" /></div>
        {err && <Note tone="bad">{err}</Note>}
        <div className="row account-foot" style={{ justifyContent: 'flex-end' }}><Button type="submit" loading={change.isPending} disabled={!valid}>修改密码</Button></div>
      </form>
    </Panel>
  );
}

/* ================================================================ 登录会话 */
/** User-Agent 的简短说法：浏览器 · 系统（Chrome · Windows）；认不出浏览器的写「其它浏览器」，原文在悬停里 */
function uaLabel(ua: string | null): string {
  if (!ua) return '未知设备';
  const b = /Edg\//.test(ua) ? 'Edge' : /Firefox\//.test(ua) ? 'Firefox' : /Chrome\//.test(ua) ? 'Chrome' : /Safari\//.test(ua) ? 'Safari' : null;
  const os = /iPhone|iPad/.test(ua) ? 'iOS' : /Android/.test(ua) ? 'Android' : /Windows/.test(ua) ? 'Windows' : /Mac OS X|Macintosh/.test(ua) ? 'macOS' : /Linux/.test(ua) ? 'Linux' : null;
  return [b ?? (/curl|python|Go-http|okhttp/i.test(ua) ? '脚本' : '其他浏览器'), os].filter(Boolean).join(' · ');
}

const SESSION_CAP = 8;
function SessionsPanel() {
  const q = useSessions(); const revoke = useRevokeSession();
  const [ask, setAsk] = React.useState<AccountSession | null>(null);
  const [others, setOthers] = React.useState(false);
  const askHeld = useHeld(ask?.id, ask);
  const list = q.data ?? [];
  const nOthers = list.filter((s) => !s.current).length;
  // 会话可能有一二十个（多是开启设备记录之前的「未知设备」）：先露前 SESSION_CAP 个（按服务端的顺序，最近活动的在前），表尾展开全部
  const [all, setAll] = React.useState(false);
  const shown = all ? list : list.slice(0, SESSION_CAP);
  return (
    <>
      <Panel title="登录会话" count={list.length} flush
        tools={<Button variant="outline" size="sm" disabled={nOthers === 0} onClick={() => setOthers(true)}><LogOut />退出其他设备</Button>}>
        {q.isError && <div style={{ padding: 'var(--sp-12) var(--card-pad)' }}><ErrorNote error={q.error} onRetry={() => void q.refetch()} /></div>}
        {q.isLoading ? <SkeletonRows rows={2} head /> : q.isError && !q.data ? null : list.length === 0 ? <Empty icon={<MonitorSmartphone size={22} />} text="没有登录会话" /> : (
          <div className="table-wrap">
            <table className="dt is-one" style={{ minWidth: 780 }}>
              <colgroup><col style={{ width: 260 }} /><col style={{ width: 170 }} /><col style={{ width: 112 }} /><col style={{ width: 128 }} /><col style={{ width: 112 }} /><col style={{ width: 52 }} /></colgroup>
              <thead><tr><th>设备</th><th>IP</th><th>登录</th><th>最近活动</th><th>到期</th><th /></tr></thead>
              <tbody>
                {shown.map((s) => (
                  <tr key={s.id}>
                    <td>
                      {/* 原始 User-Agent 只在悬停里 */}
                      <div className="row" style={{ flexWrap: 'nowrap', gap: 8 }}><span className="cell-main strong" data-tip={s.user_agent ?? '没有记录浏览器信息'}>{uaLabel(s.user_agent)}</span>{s.current && <Chip>本设备</Chip>}</div>
                    </td>
                    <td>{s.ip ? <Trunc>{s.ip}</Trunc> : <span className="t-sub">未知</span>}</td>
                    <td>{F.dt(s.created_at)}</td>
                    <td>{F.dt(s.last_seen_at)}<span className="sub">{F.ago(s.last_seen_at)}</span></td>
                    <td>{F.dt(s.expires_at)}</td>
                    <td className="c">{!s.current && <IconButton label="撤销会话" size="icon-sm" onClick={() => setAsk(s)}><LogOut size={14} /></IconButton>}</td>
                  </tr>
                ))}
              </tbody>
            </table>
            <ShowAll total={list.length} cap={SESSION_CAP} open={all} onToggle={() => setAll((v) => !v)} unit="个" />
          </div>
        )}
      </Panel>
      <ConfirmDialog open={!!ask} onOpenChange={(o) => !o && setAsk(null)} confirmLabel="撤销" danger
        title={`撤销会话 · ${askHeld ? uaLabel(askHeld.user_agent) : ''}`}
        description={`${askHeld?.ip ? `来自 ${askHeld.ip}，` : ''}那台设备需要重新登录`}
        onConfirm={async () => { if (ask) { await revoke.mutateAsync(ask.id); toast.ok('会话已撤销'); } }} />
      <RevokeOthersDialog open={others} n={nOthers} onClose={() => setOthers(false)} />
    </>
  );
}

/** 退出其他设备：要当前密码（开了两步验证还要验证码），拿到会话的人不能借它把本人踢下线 */
const RevokeOthersDialog = resetOnOpen(function RevokeOthersDialog({ open, n, onClose }: { open: boolean; n: number; onClose: () => void }) {
  const others = useRevokeOtherSessions(); const me = useMe(); const totp = useTotpStatus().data?.enabled ?? false;
  const [pw, setPw] = React.useState(''); const [code, setCode] = React.useState('');
  const [err, setErr] = React.useState<string | null>(null);
  const submit = () => {
    setErr(null);
    others.mutate({ password: pw, ...(totp ? { totp: code.trim() } : {}) }, {
      onSuccess: (r) => { onClose(); toast.ok(`已退出 ${r.revoked} 个设备`); },
      onError: (e) => { setErr(msg(e)); setCode(''); },
    });
  };
  return (
    <FormDialog size="sm" open={open} onOpenChange={(o) => !o && onClose()} title={`退出其他 ${n} 个设备？`} desc="那些设备需要重新登录"
      submitLabel="全部退出" onSubmit={submit} busy={others.isPending} valid={!!pw && (!totp || !!code.trim())} dirty={!!pw || !!code}>
      <input type="text" autoComplete="username" value={me.data?.username ?? ''} readOnly hidden />
      <FormField label="当前密码"><Input type="password" value={pw} onChange={(e) => setPw(e.target.value)} autoComplete="current-password" autoFocus /></FormField>
      {totp && <FormField label="两步验证码"><CodeInput value={code} onChange={setCode} recovery /></FormField>}
      {err && <Note tone="bad">{err}</Note>}
    </FormDialog>
  );
});
