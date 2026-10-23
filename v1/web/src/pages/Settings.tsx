// 设置：API 密钥（创建时 token 只显示一次）、告警通知渠道（Telegram / 企业微信 / Webhook，可发测试、看最近发送记录）、
// 账户安全（两步验证、修改密码、登录会话，见 features/AccountTab）、数据保留（features/RetentionTab）、汇率（计费折成美元，见 features/FxTab）、探测点与主探测来源（features/ProbeTab）、三网 PING 的全局目标（features/PingTab）。
import * as React from 'react';
import { useSearchParams } from 'react-router-dom';
import { Ban, Bell, BellOff, Eye, EyeOff, FlaskConical, KeyRound, MessagesSquare, Pencil, Plus, Send, ShieldAlert, Trash2, Webhook } from 'lucide-react';
import { RetentionTab } from '@/features/RetentionTab';
import { FxTab } from '@/features/FxTab';
import { ProbeTab } from '@/features/ProbeTab';
import { PingTab } from '@/features/PingTab';
import { AccountTab } from '@/features/AccountTab';
import { useAddChannel, useApiKeys, useCreateApiKey, useDelChannel, useMe, useNotifyChannels, useNotifyLog, usePatchChannel, useRevokeApiKey, useTestChannel, useTotpStatus } from '@/lib/queries';
import type { ApiKey, NotifyChannel, NotifyLogEntry } from '@/lib/types';
import * as F from '@/lib/format';
import { Button, IconButton } from '@/components/ui/button';
import { Checkbox, Field, InfoTip, Input, Switch } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Tabs, TabsList, TabsTrigger, Trunc } from '@/components/ui/overlays';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogTitle, DialogHeader } from '@/components/ui/dialog';
import { SkeletonRows } from '@/components/ui/skeleton';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog, FormDialog } from '@/components/app/dialogs';
import { Chip, CopyButton, Empty, ErrorNote, Note, PageHead, Panel, Pill, TableWrap } from '@/components/app/primitives';
import { ShowAll } from '@/components/app/Pagination';
import { copyText } from '@/lib/clipboard';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import '@/styles/settings.css';

export function SettingsPage() {
  const [sp, setSp] = useSearchParams();
  const sec = sp.get('section');
  const tab = sec === 'notify' || sec === 'account' || sec === 'retention' || sec === 'fx' || sec === 'probes' || sec === 'ping' ? sec : 'keys';
  return (
    <div className="stack">
      <PageHead title="设置" />
      <Tabs value={tab} onValueChange={(v) => setSp((p) => { const n = new URLSearchParams(p); n.set('section', v); return n; }, { replace: true })}>
        <TabsList className="set-tabs">
          <TabsTrigger value="keys" noPanel>API 密钥</TabsTrigger>
          <TabsTrigger value="notify" noPanel>告警通知</TabsTrigger>
          <TabsTrigger value="account" noPanel>账户安全</TabsTrigger>
          <TabsTrigger value="retention" noPanel>数据保留</TabsTrigger>
          <TabsTrigger value="fx" noPanel>汇率</TabsTrigger>
          <TabsTrigger value="probes" noPanel>探测点</TabsTrigger>
          <TabsTrigger value="ping" noPanel>三网 PING</TabsTrigger>
        </TabsList>
      </Tabs>
      {tab === 'keys' ? <ApiKeysSection /> : tab === 'notify' ? <NotifySection /> : tab === 'account' ? <AccountTab /> : tab === 'fx' ? <FxTab /> : tab === 'probes' ? <ProbeTab /> : tab === 'ping' ? <PingTab /> : <RetentionTab />}
    </div>
  );
}

/* ================================================================ API 密钥 */
const SCOPES: { value: string; label: string; desc: string; sensitive?: boolean }[] = [
  { value: 'read', label: '读取', desc: '所有查询（不含凭据）' },
  { value: 'write', label: '写入', desc: '上架、改名、续费等改动' },
  { value: 'secrets', label: '凭据', desc: '读取分享链接等凭据，会记入操作记录', sensitive: true },
  { value: 'exec', label: '远程执行', desc: '在机器上执行命令，会记入操作记录', sensitive: true },
];
const NEVER = '永不过期';
const SCOPE_LABEL = Object.fromEntries(SCOPES.map((s) => [s.value, s.label]));
const EXPIRES = [{ value: '30', label: '30 天' }, { value: '90', label: '90 天' }, { value: '365', label: '1 年' }, { value: 'never', label: NEVER }];

function keyStatus(k: ApiKey) {
  if (k.revoked_at) return <Pill tone="neutral">已撤销</Pill>;
  if (k.expires_at && new Date(k.expires_at).getTime() < Date.now()) return <Pill tone="warn">已过期</Pill>;
  return <Pill tone="ok">有效</Pill>;
}

function ApiKeysSection() {
  const q = useApiKeys();
  const create = useCreateApiKey(); const revoke = useRevokeApiKey();
  const [open, setOpen] = React.useState(false);
  const [name, setName] = React.useState(''); const [scopes, setScopes] = React.useState<string[]>(['read']); const [exp, setExp] = React.useState('90');
  // 新建要再输一次当前密码（开了两步验证还要验证码）：拿到会话的人不能借它建一把改密码后也不失效的密钥
  const [pw, setPw] = React.useState(''); const [code, setCode] = React.useState('');
  const totp = useTotpStatus().data?.enabled ?? false; const me = useMe();
  const [token, setToken] = React.useState<string | null>(null); // 创建成功后的明文，只在这个对话框里出现一次
  const [ask, setAsk] = React.useState<ApiKey | null>(null);
  // 关闭动画期间沿用最近的值：标题、密钥框不变空
  const askHeld = useHeld(ask?.id, ask);
  const tokenHeld = useHeld(token, token);
  const copyRef = React.useRef<HTMLButtonElement>(null);
  const list = q.data ?? [];
  const [err, setErr] = React.useState<string | null>(null);
  const openNew = () => { setName(''); setScopes(['read']); setExp('90'); setPw(''); setCode(''); setErr(null); setOpen(true); };
  const toggle = (s: string, on: boolean) => setScopes((l) => (on ? [...l, s] : l.filter((x) => x !== s)));
  const valid = name.trim().length > 0 && scopes.length > 0 && !!pw && (!totp || !!code.trim());
  const submit = () => {
    setErr(null);
    create.mutate({ name: name.trim(), scopes, expires_days: exp === 'never' ? undefined : Number(exp), password: pw, ...(totp ? { totp: code.trim() } : {}) }, {
      onSuccess: (r) => { setOpen(false); setPw(''); setCode(''); setToken(r.token); },
      // 密码 / 验证码不对：留在对话框里说（401，code 不是 unauthorized，不会跳登录页）
      onError: (e) => { setErr(e instanceof Error ? e.message : String(e)); setCode(''); },
    });
  };
  return (
    <>
      <Panel title="API 密钥" count={list.length} sub={<InfoTip text="给脚本和命令行调用中控用。只有登录后才能管理密钥" />} flush
        tools={<Button variant="primary" onClick={openNew}><Plus />新建密钥</Button>}>
        {q.isError && <div style={{ padding: 'var(--sp-12) var(--card-pad)' }}><ErrorNote error={q.error} onRetry={() => void q.refetch()} /></div>}
        {q.isLoading ? <SkeletonRows rows={3} head /> : list.length === 0 && !q.isError ? (
          <Empty icon={<KeyRound size={22} />} text="还没有 API 密钥"><Button variant="primary" onClick={openNew}><Plus />新建密钥</Button></Empty>
        ) : (
          <TableWrap>
            <table className="dt" style={{ minWidth: 860 }}>
              <colgroup><col style={{ width: 200 }} /><col style={{ width: 230 }} /><col style={{ width: 112 }} /><col style={{ width: 112 }} /><col style={{ width: 112 }} /><col style={{ width: 88 }} /><col style={{ width: 52 }} /></colgroup>
              <thead><tr><th>名称</th><th>权限</th><th>创建</th><th>最近使用</th><th>到期</th><th>状态</th><th /></tr></thead>
              <tbody>
                {list.map((k) => (
                  <tr key={k.id} className={k.revoked_at ? 'is-retired' : undefined}>
                    <td><Trunc className="cell-main strong">{k.name}</Trunc><span className="sub mono">flk_{k.prefix}…</span></td>
                    <td><span className="scope-tags">{k.scopes.map((s) => <Chip key={s} variant={s === 'exec' || s === 'secrets' ? 'warn' : 'neutral'}>{SCOPE_LABEL[s] ?? s}</Chip>)}</span></td>
                    <td>{F.dt(k.created_at)}</td>
                    <td>{k.last_used_at ? <>{F.dt(k.last_used_at)}<span className="sub">{F.ago(k.last_used_at)}</span></> : <span className="t-sub">从未使用</span>}</td>
                    <td>{k.expires_at ? F.dt(k.expires_at) : <span className="t-sub">{NEVER}</span>}</td>
                    <td>{keyStatus(k)}</td>
                    <td className="c">{!k.revoked_at && <IconButton label="撤销密钥" size="sm" onClick={() => setAsk(k)}><Ban size={14} /></IconButton>}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </TableWrap>
        )}
      </Panel>

      <FormDialog size="md" dirty={!!name.trim() || scopes.join() !== 'read' || exp !== '90' || !!pw} open={open} onOpenChange={setOpen} title="新建 API 密钥" desc="创建后密钥只显示一次" submitLabel="创建密钥" busy={create.isPending} valid={valid} onSubmit={submit}>
        <div className="form-grid">
          <Field label="名称"><Input value={name} onChange={(e) => setName(e.target.value)} placeholder="例如：备份脚本" autoFocus autoComplete="off" /></Field>
          <Field label="有效期"><Select value={exp} onChange={setExp} options={EXPIRES} /></Field>
        </div>
        <div className="field">
          <span className="label">权限</span>
          {/* 四项权限排成 2×2（自适应列宽在对话框里会排成 3+1） */}
          <div className="scope-list" style={{ gridTemplateColumns: 'repeat(2, minmax(0, 1fr))' }}>
            {SCOPES.map((s) => (
              <label key={s.value} className="scope-row" data-on={scopes.includes(s.value) ? '1' : undefined}>
                <Checkbox checked={scopes.includes(s.value)} onCheckedChange={(v) => toggle(s.value, v === true)} aria-label={s.label} />
                <span className="scope-name"><b>{s.label}</b><InfoTip text={s.desc} />{s.sensitive && <Chip variant="warn">敏感</Chip>}</span>
              </label>
            ))}
          </div>
          {scopes.length === 0 && <div className="hint is-error">至少选择一项权限</div>}
        </div>
        {(scopes.includes('exec') || scopes.includes('secrets')) && <Note tone="warn" icon={<ShieldAlert size={14} />}>高权限密钥，请设有效期 <InfoTip text={`这个密钥可以${[scopes.includes('exec') && '在机器上执行命令', scopes.includes('secrets') && '读取分享链接等凭据'].filter(Boolean).join('、')}，泄露的风险很高，请只放在可信的环境里`} /></Note>}
        <input type="text" autoComplete="username" value={me.data?.username ?? ''} readOnly hidden />
        <div className="form-grid">
          <Field label="当前密码"><Input type="password" value={pw} onChange={(e) => setPw(e.target.value)} autoComplete="current-password" /></Field>
          {totp && <Field label="两步验证码"><Input value={code} onChange={(e) => setCode(e.target.value)} autoComplete="one-time-code" spellCheck={false} className="mono" maxLength={20} placeholder="6 位验证码或恢复码" data-1p-ignore data-lpignore="true" /></Field>}
        </div>
        {err && <Note tone="bad">{err}</Note>}
      </FormDialog>

      <Dialog open={token != null} onOpenChange={(o) => !o && setToken(null)}>
        <DialogContent size="md" onInteractOutside={(e) => e.preventDefault()} onEscapeKeyDown={(e) => e.preventDefault()} onOpenAutoFocus={(e) => { e.preventDefault(); copyRef.current?.focus(); }}>
          <DialogHeader><DialogTitle>密钥已创建</DialogTitle><DialogDescription className="sr-only">复制并保存这个密钥</DialogDescription></DialogHeader>
          <Note tone="warn" icon={<ShieldAlert size={14} />}><b>只显示这一次</b> <InfoTip text="关掉后就看不到了。丢了只能撤销，再新建一个" /></Note>
          <div className="token-box"><code>{tokenHeld}</code><CopyButton text={tokenHeld ?? ''} label="密钥" size={15} /></div>
          <DialogFooter>
            <Button ref={copyRef} onClick={() => void copyText(token ?? '', '密钥')}>复制密钥</Button>
            <Button variant="primary" onClick={() => setToken(null)}>我已保存</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <ConfirmDialog open={!!ask} onOpenChange={(o) => !o && setAsk(null)} title={`撤销密钥「${askHeld?.name ?? ''}」？`} confirmLabel="撤销" danger
        description={<>用它的脚本会立即失效，无法恢复 <InfoTip text="脚本会收到 401 错误。需要的话，只能新建一个" /></>}
        onConfirm={async () => { await revoke.mutateAsync(ask!.id); toast.ok('密钥已撤销'); }} />
    </>
  );
}

/* ================================================================ 告警通知 */
const KINDS: Record<string, { label: string; icon: React.ReactNode; fields: { key: string; label: string; ph: string; secret?: boolean }[] }> = {
  telegram: { label: 'Telegram', icon: <Send />, fields: [{ key: 'bot_token', label: 'Bot Token', ph: '123456:ABC-DEF…', secret: true }, { key: 'chat_id', label: 'Chat ID', ph: '-1001234567890' }] },
  wecom: { label: '企业微信群机器人', icon: <MessagesSquare />, fields: [{ key: 'url', label: 'Webhook 地址', ph: 'https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=…', secret: true }] },
  webhook: { label: 'Webhook', icon: <Webhook />, fields: [{ key: 'url', label: '接收地址', ph: 'https://hooks.example.com/fleet', secret: true }] },
};
const SEVERITIES = [{ value: 'info', label: '提示及以上（全部）' }, { value: 'warning', label: '警告及以上' }, { value: 'critical', label: '只发严重' }];
/** 发送记录的种类（服务端 notify::Kind） */
// 静默变更、自动回收的对象名（subject_name）就是「静默 / 维护变更」「自动回收」，不再加前缀
const NOTIFY_KIND: Record<NotifyLogEntry['kind'], string> = { firing: '触发', repeat: '提醒', resolved: '恢复', test: '测试', silence: '', auto: '' };
const sevPill = (s: string) => (s === 'critical' ? <Pill tone="bad" dot="none">严重</Pill> : s === 'warning' ? <Pill tone="warn" dot="none">警告及以上</Pill> : <Pill tone="info" dot="none">全部</Pill>);

interface TestResult { ok: boolean; error: string | null }
const LOG_CAP = 20;

function NotifySection() {
  const q = useNotifyChannels(); const log = useNotifyLog();
  const patch = usePatchChannel(); const del = useDelChannel(); const test = useTestChannel();
  const [edit, setEdit] = React.useState<NotifyChannel | 'new' | null>(null);
  // 关闭动画期间沿用最近的对象，标题和字段不闪变
  const editHeld = useHeld(edit === 'new' ? 'new' : edit?.id, edit);
  const [ask, setAsk] = React.useState<NotifyChannel | null>(null);
  const askHeld = useHeld(ask?.id, ask);
  const [results, setResults] = React.useState<Record<string, TestResult>>({});
  const list = q.data ?? [];
  // 发送记录先露前 LOG_CAP 条，表尾展开全部（不在面板里再套一层滚动）
  const [allLog, setAllLog] = React.useState(false);
  const runTest = (c: NotifyChannel) => {
    test.mutate(c.id, {
      onSuccess: (r) => { setResults((m) => ({ ...m, [c.id]: r })); if (r.ok) toast.ok(`已向「${c.name}」发送测试消息`); else toast.err(`「${c.name}」发送失败：${r.error ?? '未知错误'}`); },
      onError: (e) => { setResults((m) => ({ ...m, [c.id]: { ok: false, error: (e as Error).message } })); toast.err(e); },
    });
  };
  return (
    <>
      <Panel title="告警通知渠道" count={list.length} sub={<InfoTip text="告警达到「最低级别」时，会推送到这些渠道" />} flush
        tools={<Button variant="primary" onClick={() => setEdit('new')}><Plus />新建渠道</Button>}>
        {q.isError && <div style={{ padding: 'var(--sp-12) var(--card-pad)' }}><ErrorNote error={q.error} onRetry={() => void q.refetch()} /></div>}
        {q.isLoading ? <SkeletonRows rows={3} head /> : list.length === 0 && !q.isError ? (
          // 没有渠道 = 所有告警都不会推给任何人：用警示色说清后果，不是和「还没有 API 密钥」一样分量的普通空状态（新建在标题栏右边，不重复放按钮）
          <div style={{ padding: 'var(--sp-12) var(--card-pad)' }}><Note tone="warn" icon={<BellOff size={14} />}>还没有通知渠道，告警不会推送给任何人 <InfoTip text="告警照常记在「告警」页，只是不会发出去。用右上角「新建渠道」添加 Telegram、企业微信或 Webhook" /></Note></div>
        ) : (
          <TableWrap>
            <table className="dt" style={{ minWidth: 860 }}>
              <colgroup><col style={{ width: 250 }} /><col style={{ width: 150 }} /><col style={{ width: 70 }} /><col style={{ width: 200 }} /><col style={{ width: 130 }} /></colgroup>
              <thead><tr><th>渠道</th><th>最低级别</th><th>启用</th><th>测试</th><th /></tr></thead>
              <tbody>
                {list.map((c) => {
                  const k = KINDS[c.kind]; const r = results[c.id];
                  return (
                    <tr key={c.id}>
                      <td><div className="row" style={{ flexWrap: 'nowrap', gap: 10 }}><span className="kind-ic" aria-hidden>{k?.icon ?? <Bell />}</span><div style={{ minWidth: 0 }}><Trunc className="cell-main strong">{c.name}</Trunc><span className="sub">{k?.label ?? c.kind} · {c.config_hint}</span></div></div></td>
                      <td>{sevPill(c.min_severity)}</td>
                      <td><Switch checked={c.enabled} disabled={patch.isPending && patch.variables?.id === c.id} aria-label={`${c.enabled ? '停用' : '启用'}「${c.name}」`}
                        onCheckedChange={(v) => patch.mutate({ id: c.id, enabled: v }, { onSuccess: () => toast.ok(v ? '渠道已启用' : '渠道已停用'), onError: toast.err })} /></td>
                      <td>
                        <span className="row" style={{ flexWrap: 'nowrap', gap: 8 }}>
                          <Button size="sm" loading={test.isPending && test.variables === c.id} disabled={test.isPending} onClick={() => runTest(c)}><FlaskConical size={13} />发送测试</Button>
                          {r && (r.ok
                            ? <span className="test-result" style={{ color: 'var(--ok-text)' }} role="status">✓ 已送达</span>
                            : <span className="test-result" style={{ color: 'var(--bad-text)', minWidth: 0 }} role="alert"><Trunc text={`失败：${r.error ?? '未知错误'}`}>✗ 失败：{r.error ?? '未知错误'}</Trunc></span>)}
                        </span>
                      </td>
                      <td className="r"><span className="row" style={{ justifyContent: 'flex-end', flexWrap: 'nowrap', gap: 0 }}>
                        <IconButton label="编辑渠道" size="sm" onClick={() => setEdit(c)}><Pencil size={14} /></IconButton>
                        <IconButton label="删除渠道" size="sm" onClick={() => setAsk(c)}><Trash2 size={14} /></IconButton>
                      </span></td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </TableWrap>
        )}
      </Panel>

      <Panel title="最近发送记录" count={log.data?.length} sub={<InfoTip text="最近 100 条" />} flush>
        {log.isError && <div style={{ padding: 'var(--sp-12) var(--card-pad)' }}><ErrorNote error={log.error} onRetry={() => void log.refetch()} /></div>}
        {log.isLoading ? <SkeletonRows rows={3} one head /> : !log.data?.length && !log.isError ? <Empty icon={<Send size={22} />} text="还没有发送记录" /> : (
          <TableWrap>
            <table className="dt is-one" style={{ minWidth: 640 }}>
              <colgroup><col style={{ width: 120 }} /><col style={{ width: 200 }} /><col style={{ width: 90 }} /><col style={{ width: 240 }} /><col /></colgroup>
              <thead><tr><th>时间</th><th>渠道</th><th>结果</th><th>来源</th><th>错误</th></tr></thead>
              <tbody>
                {(allLog ? log.data ?? [] : (log.data ?? []).slice(0, LOG_CAP)).map((l) => (
                  <tr key={l.id}>
                    {/* 渠道删了记录还在（名字随渠道一起没了）：写明是已删除的渠道 */}
                    <td data-tip={F.dtFull(l.ts)}>{F.dt(l.ts)}</td><td>{l.channel_name ? <Trunc className="cell-main strong">{l.channel_name}</Trunc> : <span className="cell-main t-muted">已删除的渠道</span>}</td>
                    <td>{l.ok ? <Pill tone="ok">成功</Pill> : <Pill tone="bad">失败</Pill>}</td>
                    <td>{l.kind === 'test' ? <span className="t-sub">测试消息</span> : <Trunc className="cell-main">{[NOTIFY_KIND[l.kind], l.subject_name ?? '（机器已删除）', l.rule_name].filter(Boolean).join(' · ')}</Trunc>}</td>
                    <td><Trunc className="cell-main" text={l.error ?? ''}>{l.error ? <span style={{ color: 'var(--bad-text)' }}>{l.error}</span> : <span className="t-muted">—</span>}</Trunc></td>
                  </tr>
                ))}
              </tbody>
            </table>
            <ShowAll total={log.data?.length ?? 0} cap={LOG_CAP} open={allLog} onToggle={() => setAllLog((v) => !v)} />
          </TableWrap>
        )}
      </Panel>

      {editHeld && <ChannelDialog target={editHeld} open={edit != null} onClose={() => setEdit(null)} />}
      <ConfirmDialog open={!!ask} onOpenChange={(o) => !o && setAsk(null)} title={`删除渠道「${askHeld?.name ?? ''}」？`} confirmLabel="删除" danger
        description="发送记录会保留"
        onConfirm={async () => { await del.mutateAsync(ask!.id); toast.ok('渠道已删除'); }} />
    </>
  );
}

/** 新建 / 编辑渠道。每次打开重新挂载（resetOnOpen）：字段直接按这次的渠道初始化，配置留空 */
const ChannelDialog = resetOnOpen(function ChannelDialog({ target, open, onClose }: { target: NotifyChannel | 'new'; open: boolean; onClose: () => void }) {
  const add = useAddChannel(); const patch = usePatchChannel();
  const ch = target === 'new' ? null : target;
  const isNew = !ch;
  const [name, setName] = React.useState(ch?.name ?? ''); const [kind, setKind] = React.useState(ch?.kind ?? 'telegram');
  const [cfg, setCfg] = React.useState<Record<string, string>>({}); const [sev, setSev] = React.useState(ch?.min_severity ?? 'warning'); const [enabled, setEnabled] = React.useState(ch?.enabled ?? true);
  const [touched, setTouched] = React.useState(false); // 动过配置字段后才提示“填写完整”，不一打开就标红
  const [reveal, setReveal] = React.useState(false);
  const k = KINDS[kind];
  const filled = k.fields.filter((f) => (cfg[f.key] ?? '').trim() !== '').length;
  // 配置整体替换：新建必须填全；编辑时留空 = 不改，只填一部分则要求补全
  const cfgOk = isNew ? filled === k.fields.length : filled === 0 || filled === k.fields.length;
  const valid = name.trim().length > 0 && cfgOk;
  const config = Object.fromEntries(k.fields.map((f) => [f.key, (cfg[f.key] ?? '').trim()]));
  const busy = add.isPending || patch.isPending;
  const dirty = filled > 0 || (ch ? name !== ch.name || sev !== ch.min_severity || enabled !== ch.enabled : name.trim() !== '');
  const submit = () => {
    if (ch) patch.mutate({ id: ch.id, name: name.trim(), min_severity: sev, enabled, ...(filled ? { config } : {}) }, { onSuccess: () => { toast.ok('渠道已保存'); onClose(); }, onError: toast.err });
    else add.mutate({ name: name.trim(), kind, config, min_severity: sev, enabled }, { onSuccess: () => { toast.ok('渠道已创建'); onClose(); }, onError: toast.err });
  };
  return (
    <FormDialog size="md" dirty={dirty} open={open} onOpenChange={(o) => !o && onClose()} title={ch ? `编辑渠道 · ${ch.name}` : '新建通知渠道'} desc={<>配置会加密保存 <InfoTip text="保存后，页面上不再显示原文" /></>}
      submitLabel={ch ? '保存' : '创建'} busy={busy} valid={valid} onSubmit={submit}>
      <div className="form-grid">
        <Field label="名称"><Input value={name} onChange={(e) => setName(e.target.value)} placeholder="例如：运维群" autoFocus autoComplete="off" /></Field>
        <Field label="类型" hint={ch ? '类型创建后不能改' : undefined}><Select value={kind} onChange={(v) => { setKind(v); setCfg({}); }} disabled={!!ch} options={Object.entries(KINDS).map(([value, x]) => ({ value, label: x.label }))} /></Field>
      </div>
      <div className="form-grid">
        {k.fields.map((f) => (
          <Field key={f.key} label={f.label} className={k.fields.length === 1 ? 'span-2' : undefined} >
            {/* 密钥类字段默认遮住，旁边可切换显示（核对粘贴进来的地址、token） */}
            <div className="row" style={{ flexWrap: 'nowrap', gap: 'var(--sp-6)' }}>
              <Input type={f.secret && !reveal ? 'password' : 'text'} value={cfg[f.key] ?? ''} onChange={(e) => { setCfg((c) => ({ ...c, [f.key]: e.target.value })); setTouched(true); }} placeholder={ch ? `留空不改（当前：${ch.config_hint}）` : f.ph} autoComplete="off" spellCheck={false} data-1p-ignore data-lpignore="true" />
              {f.secret && <IconButton label={reveal ? '隐藏' : '显示'} aria-pressed={reveal} onClick={() => setReveal((v) => !v)}>{reveal ? <EyeOff size={15} /> : <Eye size={15} />}</IconButton>}
            </div>
          </Field>
        ))}
      </div>
      {!cfgOk && touched && <div className="hint is-error">{isNew ? '请把配置填写完整' : '要改配置，请把所有字段重新填上'}</div>}
      <div className="form-grid">
        <Field label="最低级别"><Select value={sev} onChange={setSev} options={SEVERITIES} /></Field>
        <Field label="状态"><label className="check-row" style={{ gap: 10, height: 'var(--h-lg)' }}><Switch checked={enabled} onCheckedChange={setEnabled} /><span>{enabled ? '启用' : '已停用'}</span></label></Field>
      </div>
    </FormDialog>
  );
});
