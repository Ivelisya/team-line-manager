// 设置：API 密钥（创建时 token 只显示一次）与告警通知渠道（Telegram / 企业微信 / Webhook，可发测试、看最近发送记录）。
import * as React from 'react';
import { useSearchParams } from 'react-router-dom';
import { Ban, Bell, FlaskConical, KeyRound, MessagesSquare, Pencil, Plus, Send, ShieldAlert, Trash2, Webhook } from 'lucide-react';
import { useAddChannel, useApiKeys, useCreateApiKey, useDelChannel, useNotifyChannels, useNotifyLog, usePatchChannel, useRevokeApiKey, useTestChannel } from '@/lib/queries';
import type { ApiKey, NotifyChannel, NotifyLogEntry } from '@/lib/types';
import * as F from '@/lib/format';
import { Button, IconButton } from '@/components/ui/button';
import { Checkbox, Field, Input, Switch } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Tabs, TabsList, TabsTrigger, Trunc } from '@/components/ui/overlays';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogTitle, DialogHeader } from '@/components/ui/dialog';
import { SkeletonRows } from '@/components/ui/skeleton';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog, FormDialog } from '@/components/app/dialogs';
import { Chip, CopyButton, Empty, ErrorNote, Note, PageHead, Panel, Pill } from '@/components/app/primitives';
import { copyText } from '@/lib/clipboard';
import { resetOnOpen, useHeld } from '@/lib/useHeld';

export function SettingsPage() {
  const [sp, setSp] = useSearchParams();
  const tab = sp.get('section') === 'notify' ? 'notify' : 'keys';
  return (
    <div className="stack">
      <PageHead title="设置" note="API 密钥与告警通知" />
      <Tabs value={tab} onValueChange={(v) => setSp((p) => { const n = new URLSearchParams(p); n.set('section', v); return n; }, { replace: true })}>
        <TabsList className="set-tabs">
          <TabsTrigger value="keys" noPanel><KeyRound size={14} />API 密钥</TabsTrigger>
          <TabsTrigger value="notify" noPanel><Bell size={14} />告警通知</TabsTrigger>
        </TabsList>
      </Tabs>
      {tab === 'keys' ? <ApiKeysSection /> : <NotifySection />}
    </div>
  );
}

/* ================================================================ API 密钥 */
const SCOPES: { value: string; label: string; desc: string; sensitive?: boolean }[] = [
  { value: 'read', label: '读取', desc: '所有查询接口（不含分享链接这类凭据）' },
  { value: 'write', label: '写入', desc: '上架、改名、续费、静默等会改动数据的操作' },
  { value: 'secrets', label: '凭据', desc: '读取入口分享链接等凭据；每次读取都会记入审计', sensitive: true },
  { value: 'exec', label: '远程执行', desc: '在机器上执行命令；命令全文会记入审计', sensitive: true },
];
const SCOPE_LABEL = Object.fromEntries(SCOPES.map((s) => [s.value, s.label]));
const EXPIRES = [{ value: '30', label: '30 天' }, { value: '90', label: '90 天' }, { value: '365', label: '1 年' }, { value: 'never', label: '永不过期' }];

function keyStatus(k: ApiKey) {
  if (k.revoked_at) return <Pill tone="neutral" dot="ring">已撤销</Pill>;
  if (k.expires_at && new Date(k.expires_at).getTime() < Date.now()) return <Pill tone="warn">已过期</Pill>;
  return <Pill tone="ok">有效</Pill>;
}

function ApiKeysSection() {
  const q = useApiKeys();
  const create = useCreateApiKey(); const revoke = useRevokeApiKey();
  const [open, setOpen] = React.useState(false);
  const [name, setName] = React.useState(''); const [scopes, setScopes] = React.useState<string[]>(['read']); const [exp, setExp] = React.useState('90');
  const [token, setToken] = React.useState<string | null>(null); // 创建成功后的明文，只在这个对话框里出现一次
  const [ask, setAsk] = React.useState<ApiKey | null>(null);
  // 关闭动画期间沿用最近的值：标题、密钥框不变空
  const askHeld = useHeld(ask?.id, ask);
  const tokenHeld = useHeld(token, token);
  const copyRef = React.useRef<HTMLButtonElement>(null);
  const list = q.data ?? [];
  const openNew = () => { setName(''); setScopes(['read']); setExp('90'); setOpen(true); };
  const toggle = (s: string, on: boolean) => setScopes((l) => (on ? [...l, s] : l.filter((x) => x !== s)));
  const valid = name.trim().length > 0 && scopes.length > 0;
  const submit = () => create.mutate({ name: name.trim(), scopes, expires_days: exp === 'never' ? undefined : Number(exp) }, {
    onSuccess: (r) => { setOpen(false); setToken(r.token); }, onError: toast.err,
  });
  return (
    <>
      <Panel title="API 密钥" count={list.length} sub="给脚本和运维命令行调用中控用；只有登录会话能管理密钥" flush
        tools={<Button variant="primary" onClick={openNew}><Plus />新建密钥</Button>}>
        {q.isError && <div style={{ padding: 'var(--sp-12) var(--card-pad)' }}><ErrorNote error={q.error} onRetry={() => void q.refetch()} /></div>}
        {q.isLoading ? <SkeletonRows rows={3} head /> : list.length === 0 && !q.isError ? (
          <Empty icon={<KeyRound size={22} />} text="还没有 API 密钥" hint="新建一个，用 Authorization: Bearer 调用接口"><Button variant="primary" onClick={openNew}><Plus />新建密钥</Button></Empty>
        ) : (
          <div className="table-wrap">
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
                    <td>{k.expires_at ? F.dt(k.expires_at) : <span className="t-sub">永不</span>}</td>
                    <td>{keyStatus(k)}</td>
                    <td className="c">{!k.revoked_at && <IconButton label="撤销密钥" size="sm" onClick={() => setAsk(k)}><Ban size={14} /></IconButton>}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>

      <FormDialog size="md" open={open} onOpenChange={setOpen} title="新建 API 密钥" desc="只授予脚本需要的权限。创建后密钥只显示一次。" submitLabel="创建密钥" busy={create.isPending} valid={valid} onSubmit={submit}>
        <div className="form-grid">
          <Field label="名称" hint="例如：备份脚本、监控 CLI"><Input value={name} onChange={(e) => setName(e.target.value)} autoFocus autoComplete="off" /></Field>
          <Field label="有效期"><Select value={exp} onChange={setExp} options={EXPIRES} /></Field>
        </div>
        <div className="field">
          <span className="label">权限</span>
          <div className="scope-list">
            {SCOPES.map((s) => (
              <label key={s.value} className="scope-row" data-on={scopes.includes(s.value) ? '1' : undefined}>
                <Checkbox checked={scopes.includes(s.value)} onCheckedChange={(v) => toggle(s.value, v === true)} aria-label={s.label} />
                <span><b>{s.label}</b>{s.sensitive && <> <Chip variant="warn">敏感</Chip></>}<div className="hint">{s.desc}</div></span>
              </label>
            ))}
          </div>
          {scopes.length === 0 && <div className="hint is-error">至少选择一项权限</div>}
        </div>
        {(scopes.includes('exec') || scopes.includes('secrets')) && <Note tone="warn" icon={<ShieldAlert size={14} />}>这个密钥可以{scopes.includes('exec') ? '在机器上执行命令' : ''}{scopes.includes('exec') && scopes.includes('secrets') ? '、' : ''}{scopes.includes('secrets') ? '读取分享链接等凭据' : ''}，泄露后风险很高。请只放在可信的环境里，并设置有效期。</Note>}
      </FormDialog>

      <Dialog open={token != null} onOpenChange={(o) => !o && setToken(null)}>
        <DialogContent size="md" onInteractOutside={(e) => e.preventDefault()} onEscapeKeyDown={(e) => e.preventDefault()} onOpenAutoFocus={(e) => { e.preventDefault(); copyRef.current?.focus(); }}>
          <DialogHeader><DialogTitle>密钥已创建</DialogTitle><DialogDescription>请现在复制并保存。</DialogDescription></DialogHeader>
          <Note tone="warn" icon={<ShieldAlert size={14} />}><b>只显示这一次。</b>关闭后无法再次查看完整密钥；丢了只能撤销后重建。</Note>
          <div className="token-box"><code>{tokenHeld}</code><CopyButton text={tokenHeld ?? ''} label="密钥" size={15} /></div>
          <DialogFooter>
            <Button ref={copyRef} onClick={() => void copyText(token ?? '', '密钥')}>复制密钥</Button>
            <Button variant="primary" onClick={() => setToken(null)}>我已保存</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>

      <ConfirmDialog open={!!ask} onOpenChange={(o) => !o && setAsk(null)} title={`撤销密钥“${askHeld?.name ?? ''}”？`} confirmLabel="撤销" danger
        description="撤销后使用这个密钥的脚本会立即收到 401，无法恢复。需要的话只能新建一个。"
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
const NOTIFY_KIND: Record<NotifyLogEntry['kind'], string> = { firing: '触发', repeat: '提醒', resolved: '恢复', test: '测试' };
const sevPill = (s: string) => (s === 'critical' ? <Pill tone="bad" dot="none">严重</Pill> : s === 'warning' ? <Pill tone="warn" dot="none">警告及以上</Pill> : <Pill tone="info" dot="none">全部</Pill>);

interface TestResult { ok: boolean; error: string | null }

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
  const runTest = (c: NotifyChannel) => {
    test.mutate(c.id, {
      onSuccess: (r) => { setResults((m) => ({ ...m, [c.id]: r })); if (r.ok) toast.ok(`已向“${c.name}”发送测试消息`); else toast.err(`“${c.name}”发送失败：${r.error ?? '未知错误'}`); },
      onError: (e) => { setResults((m) => ({ ...m, [c.id]: { ok: false, error: (e as Error).message } })); toast.err(e); },
    });
  };
  return (
    <>
      <Panel title="告警通知渠道" count={list.length} sub="告警触发时按“最低级别”推送到这些渠道" flush
        tools={<Button variant="primary" onClick={() => setEdit('new')}><Plus />新建渠道</Button>}>
        {q.isError && <div style={{ padding: 'var(--sp-12) var(--card-pad)' }}><ErrorNote error={q.error} onRetry={() => void q.refetch()} /></div>}
        {q.isLoading ? <SkeletonRows rows={3} head /> : list.length === 0 && !q.isError ? (
          <Empty icon={<Bell size={22} />} text="还没有通知渠道" hint="告警现在只显示在页面里。添加一个渠道，让它主动推到你的 Telegram、企业微信或自己的服务。"><Button variant="primary" onClick={() => setEdit('new')}><Plus />新建渠道</Button></Empty>
        ) : (
          <div className="table-wrap">
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
                      <td><Switch checked={c.enabled} disabled={patch.isPending && patch.variables?.id === c.id} aria-label={`${c.enabled ? '停用' : '启用'}“${c.name}”`}
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
          </div>
        )}
      </Panel>

      <Panel title="最近发送记录" count={log.data?.length} sub="最近 100 条，含测试消息" flush>
        {log.isError && <div style={{ padding: 'var(--sp-12) var(--card-pad)' }}><ErrorNote error={log.error} onRetry={() => void log.refetch()} /></div>}
        {log.isLoading ? <SkeletonRows rows={3} one head /> : !log.data?.length && !log.isError ? <Empty icon={<Send size={22} />} text="还没有发送记录" /> : (
          <div className="table-wrap" style={{ maxHeight: 420, overflowY: 'auto' }}>
            <table className="dt is-one" style={{ minWidth: 640 }}>
              <colgroup><col style={{ width: 156 }} /><col style={{ width: 170 }} /><col style={{ width: 90 }} /><col style={{ width: 240 }} /><col /></colgroup>
              <thead><tr><th>时间</th><th>渠道</th><th>结果</th><th>来源</th><th>错误</th></tr></thead>
              <tbody>
                {(log.data ?? []).map((l) => (
                  <tr key={l.id}>
                    <td>{F.dtFull(l.ts)}</td><td className="strong"><Trunc className="cell-main">{l.channel_name}</Trunc></td>
                    <td>{l.ok ? <Pill tone="ok">成功</Pill> : <Pill tone="bad">失败</Pill>}</td>
                    <td>{l.kind === 'test' ? <span className="t-sub">测试消息</span> : <Trunc className="cell-main">{[NOTIFY_KIND[l.kind], l.subject_name ?? '（机器已删除）', l.rule_name].filter(Boolean).join(' · ')}</Trunc>}</td>
                    <td><Trunc className="cell-main" text={l.error ?? ''}>{l.error ? <span style={{ color: 'var(--bad-text)' }}>{l.error}</span> : <span className="t-muted">—</span>}</Trunc></td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>

      {editHeld && <ChannelDialog target={editHeld} open={edit != null} onClose={() => setEdit(null)} />}
      <ConfirmDialog open={!!ask} onOpenChange={(o) => !o && setAsk(null)} title={`删除渠道“${askHeld?.name ?? ''}”？`} confirmLabel="删除" danger
        description="删除后告警不会再推送到这个渠道；发送记录保留。"
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
    <FormDialog size="md" dirty={dirty} open={open} onOpenChange={(o) => !o && onClose()} title={ch ? `编辑渠道 · ${ch.name}` : '新建通知渠道'} desc="配置加密保存，保存后页面不再显示明文。"
      submitLabel={ch ? '保存' : '创建'} busy={busy} valid={valid} onSubmit={submit}>
      <div className="form-grid">
        <Field label="名称"><Input value={name} onChange={(e) => setName(e.target.value)} placeholder="例如：运维群" autoFocus autoComplete="off" /></Field>
        <Field label="类型" hint={ch ? '类型创建后不能改' : undefined}><Select value={kind} onChange={(v) => { setKind(v); setCfg({}); }} disabled={!!ch} options={Object.entries(KINDS).map(([value, x]) => ({ value, label: x.label }))} /></Field>
      </div>
      <div className="form-grid">
        {k.fields.map((f) => (
          <Field key={f.key} label={f.label} className={k.fields.length === 1 ? 'span-2' : undefined} hint={ch ? `留空则保持不变（当前：${ch.config_hint}）` : undefined}>
            <Input type={f.secret ? 'password' : 'text'} value={cfg[f.key] ?? ''} onChange={(e) => { setCfg((c) => ({ ...c, [f.key]: e.target.value })); setTouched(true); }} placeholder={f.ph} autoComplete="off" spellCheck={false} />
          </Field>
        ))}
      </div>
      {!cfgOk && touched && <div className="hint is-error">{isNew ? '请把配置填写完整' : '要修改配置，需要把这一类型的所有字段都填上'}</div>}
      <div className="form-grid">
        <Field label="最低级别" hint="低于这个级别的告警不会推送"><Select value={sev} onChange={setSev} options={SEVERITIES} /></Field>
        <Field label="状态"><label className="check-row" style={{ gap: 10, height: 'var(--h-lg)' }}><Switch checked={enabled} onCheckedChange={setEnabled} /><span>{enabled ? '启用' : '已停用'}</span></label></Field>
      </div>
    </FormDialog>
  );
});
