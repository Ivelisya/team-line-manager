// 设置「客户端订阅」：给自己的客户端（Windows 上的 Clash Verge、OpenClash 的 proxy-provider、v2rayN / Shadowrocket）一个链接，
// 按规则（团队、指定机器）在拉取时现算在用的入口。列表（规则、条目数、最近拉取）、新建（链接只显示一次）、改规则、启用 / 停用、轮换链接、删除、看条目。
import * as React from 'react';
import { List, Pencil, Plus, RefreshCw, Rss, ShieldAlert, Trash2, X } from 'lucide-react';
import { DEFAULT_RULE, useClientSub, useClientSubs, useCreateClientSub, useDelClientSub, usePatchClientSub, useRotateClientSub, type ClientSub, type ClientSubWithUrl, type SubProtocols, type SubRule } from '@/lib/clientSubs';
import { useMachineBriefs, useTeams } from '@/lib/queries';
import * as F from '@/lib/format';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import { Button, IconButton } from '@/components/ui/button';
import { Checkbox, Field, InfoTip, Input, OptimisticSwitch, Switch } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { Trunc } from '@/components/ui/overlays';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from '@/components/ui/dialog';
import { SkeletonRows } from '@/components/ui/skeleton';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog, FormDialog } from '@/components/app/dialogs';
import { Chip, CopyButton, Empty, ErrorNote, Note, Panel, TableWrap } from '@/components/app/primitives';

const INTRO = '给自己的客户端用：Clash Verge、OpenClash 填链接，v2rayN、Shadowrocket 在链接后加 ?format=base64\n每次拉取按规则现算在用的入口，换节点、换线路机、轮换凭据后客户端更新订阅就是新的';
const PROTOS: { value: SubProtocols; label: string }[] = [{ value: 'reality', label: 'Reality' }, { value: 'both', label: 'Reality + AnyTLS' }, { value: 'anytls', label: '只 AnyTLS' }];
const protoLabel = (p: SubProtocols | undefined) => PROTOS.find((x) => x.value === p)?.label ?? 'Reality';
const MODES = [{ value: 'all', label: '全部团队（不含备用团队）' }, { value: 'except', label: '全部团队，除了…' }, { value: 'only', label: '只要这些团队' }];

export function SubsTab() {
  const q = useClientSubs();
  const patch = usePatchClientSub(); const rotate = useRotateClientSub(); const remove = useDelClientSub();
  const [edit, setEdit] = React.useState<ClientSub | 'new' | null>(null);
  const editHeld = useHeld(edit === 'new' ? 'new' : edit?.id, edit);
  const [link, setLink] = React.useState<ClientSubWithUrl | null>(null);
  const [ask, setAsk] = React.useState<{ s: ClientSub; what: 'rotate' | 'delete' } | null>(null);
  const askHeld = useHeld(ask?.s.id, ask);
  const [view, setView] = React.useState<ClientSub | null>(null);
  const list = q.data ?? [];
  const newBtn = <Button variant="primary" onClick={() => setEdit('new')}><Plus />新建订阅</Button>;
  const isEmpty = !q.isLoading && !q.isError && list.length === 0; // 空态里已有一个「新建订阅」，标题旁不再放第二个
  return (
    <>
      <Panel title="客户端订阅" count={list.length} sub={<InfoTip text={INTRO} />} flush tools={isEmpty ? undefined : newBtn}>
        {q.isError && <div style={{ padding: 'var(--sp-12) var(--card-pad)' }}><ErrorNote error={q.error} onRetry={() => void q.refetch()} /></div>}
        {q.isLoading ? <SkeletonRows rows={2} head /> : isEmpty ? (
          <Empty icon={<Rss size={22} />} text="还没有客户端订阅">{newBtn}</Empty>
        ) : (
          <TableWrap>
            <table className="dt" style={{ minWidth: 820 }}>
              <colgroup><col /><col style={{ width: 130 }} /><col style={{ width: 90 }} /><col style={{ width: 150 }} /><col style={{ width: 64 }} /><col style={{ width: 150 }} /></colgroup>
              <thead><tr><th>名称</th><th>协议</th><th className="r">入口</th><th>最近拉取</th><th>启用</th><th /></tr></thead>
              <tbody>
                {list.map((s) => (
                  <tr key={s.id} className={s.enabled ? undefined : 'is-retired'}>
                    <td><Trunc className="cell-main strong">{s.name}</Trunc><Trunc className="sub" text={s.rule_text}>{s.rule_text}</Trunc></td>
                    <td>{protoLabel(s.protocols)}</td>
                    <td className="r num">{s.items} 个</td>
                    <td>{s.last_fetch_at ? <><span data-tip={F.dt(s.last_fetch_at)}>{F.ago(s.last_fetch_at)}</span><span className="sub mono">{s.last_fetch_ip ?? ''}</span></> : <span className="t-sub">还没拉过</span>}</td>
                    <td><OptimisticSwitch checked={s.enabled} disabled={patch.isPending && patch.variables?.id === s.id} aria-label={`${s.enabled ? '停用' : '启用'}「${s.name}」`}
                      onCheckedChange={(v, undo) => patch.mutate({ id: s.id, enabled: v }, { onSuccess: () => toast.ok(v ? '订阅已启用' : '订阅已停用，拉取会失败'), onError: (e) => { undo(); toast.err(e); } })} /></td>
                    <td className="r"><span className="row" style={{ justifyContent: 'flex-end', flexWrap: 'nowrap', gap: 0 }}>
                      <IconButton label="看条目" size="sm" onClick={() => setView(s)}><List size={14} /></IconButton>
                      <IconButton label="编辑" size="sm" onClick={() => setEdit(s)}><Pencil size={14} /></IconButton>
                      <IconButton label="轮换链接" size="sm" onClick={() => setAsk({ s, what: 'rotate' })}><RefreshCw size={14} /></IconButton>
                      <IconButton label="删除" size="sm" onClick={() => setAsk({ s, what: 'delete' })}><Trash2 size={14} /></IconButton>
                    </span></td>
                  </tr>
                ))}
              </tbody>
            </table>
          </TableWrap>
        )}
      </Panel>

      {editHeld && <SubDialog target={editHeld} open={edit != null} onClose={() => setEdit(null)} onCreated={setLink} />}
      <LinkDialog sub={link} onClose={() => setLink(null)} />
      <EntriesDialog sub={view} onClose={() => setView(null)} />
      <ConfirmDialog open={!!ask} onOpenChange={(o) => !o && setAsk(null)} danger
        title={`${askHeld?.what === 'rotate' ? '轮换链接' : '删除订阅'} · ${askHeld?.s.name ?? ''}`} confirmLabel={askHeld?.what === 'rotate' ? '轮换' : '删除'}
        description={askHeld?.what === 'rotate' ? '旧链接立即失效，用它的客户端要换成新链接' : '用它的客户端之后拉不到入口'}
        onConfirm={async () => {
          if (!ask) return;
          if (ask.what === 'rotate') setLink(await rotate.mutateAsync(ask.s.id));
          else { await remove.mutateAsync(ask.s.id); toast.ok('订阅已删除'); }
        }} />
    </>
  );
}

/** 新建 / 轮换后的链接：只显示这一次 */
function LinkDialog({ sub, onClose }: { sub: ClientSubWithUrl | null; onClose: () => void }) {
  const held = useHeld(sub?.url, sub);
  const b64 = held ? `${held.url}?format=base64` : '';
  return (
    <Dialog open={!!sub} onOpenChange={(o) => !o && onClose()}>
      <DialogContent size="md" onInteractOutside={(e) => e.preventDefault()} onEscapeKeyDown={(e) => e.preventDefault()}>
        <DialogHeader><DialogTitle>订阅链接 · {held?.name}</DialogTitle><DialogDescription className="sr-only">复制并保存订阅链接</DialogDescription></DialogHeader>
        <Note tone="warn" icon={<ShieldAlert size={14} />}><b>只显示这一次</b> <InfoTip text={'链接就是凭据，拿到的人能连上这些入口\n丢了只能轮换，换成新链接'} /></Note>
        <Field label="Clash Verge / OpenClash"><div className="token-box"><code>{held?.url}</code><CopyButton text={held?.url ?? ''} label="链接" size={15} /></div></Field>
        <Field label="v2rayN / Shadowrocket"><div className="token-box"><code>{b64}</code><CopyButton text={b64} label="链接" size={15} /></div></Field>
        <DialogFooter><Button variant="primary" onClick={onClose}>我已保存</Button></DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** 现在的条目：客户端里的名字、线路机:端口 */
function EntriesDialog({ sub, onClose }: { sub: ClientSub | null; onClose: () => void }) {
  const held = useHeld(sub?.id, sub);
  const q = useClientSub(sub?.id ?? null);
  const es = q.data?.entries ?? [];
  const at = !!q.data && q.data.protocols !== 'reality';
  return (
    <Dialog open={!!sub} onOpenChange={(o) => !o && onClose()}>
      <DialogContent size="lg">
        <DialogHeader><DialogTitle>条目 · {held?.name}</DialogTitle><DialogDescription>{held?.rule_text}</DialogDescription></DialogHeader>
        {q.isError ? <ErrorNote error={q.error} onRetry={() => void q.refetch()} /> : q.isLoading ? <SkeletonRows rows={3} /> : !es.length ? <Empty text="现在没有条目" /> : (
          <TableWrap>
            <table className="dt is-one">
              <thead><tr><th>客户端里的名字</th><th>线路机:端口</th>{at && <th>AnyTLS 端口</th>}</tr></thead>
              <tbody>{es.map((e) => <tr key={e.entry_id}><td><Trunc className="cell-main">{e.name}</Trunc></td><td className="num">{e.relay}:{e.port}</td>
                {at && <td className="num">{e.anytls_port ?? <span className="t-sub" data-tip="线路机的 AnyTLS 没在跑">—</span>}</td>}</tr>)}</tbody>
            </table>
          </TableWrap>
        )}
        <DialogFooter><Button onClick={onClose}>关闭</Button></DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/** 另加 / 排除的机器：下拉加一台，标签上点 × 去掉 */
function MachinePicks({ label, info, ids, onChange, options }: { label: string; info: string; ids: string[]; onChange: (ids: string[]) => void; options: { value: string; label: string }[] }) {
  const name = (id: string) => options.find((o) => o.value === id)?.label ?? '（已删除）';
  const rest = React.useMemo(() => options.filter((o) => !ids.includes(o.value)), [options, ids]);
  const add = React.useCallback((v: string) => v && onChange([...ids, v]), [ids, onChange]);
  return (
    <Field label={label} info={info}>
      <div className="row" style={{ gap: 'var(--sp-6)', flexWrap: 'wrap' }}>
        {ids.map((id) => (
          <Chip key={id}>{name(id)}<IconButton label={`去掉 ${name(id)}`} size="sm" onClick={() => onChange(ids.filter((x) => x !== id))}><X size={12} /></IconButton></Chip>
        ))}
        <Select sm value="" onChange={add} options={rest} placeholder="加一台…" aria-label={label} />
      </div>
    </Field>
  );
}

/** 新建 / 编辑订阅。每次打开重新挂载（resetOnOpen）：字段直接按这次的订阅初始化 */
const SubDialog = resetOnOpen(function SubDialog({ target, open, onClose, onCreated }: { target: ClientSub | 'new'; open: boolean; onClose: () => void; onCreated: (s: ClientSubWithUrl) => void }) {
  const create = useCreateClientSub(); const patch = usePatchClientSub();
  const teams = useTeams().data ?? []; const machines = useMachineBriefs().data ?? [];
  const cur = target === 'new' ? null : target;
  const [name, setName] = React.useState(cur?.name ?? ''); const [rule, setRule] = React.useState<SubRule>(cur?.rule ?? DEFAULT_RULE);
  const [protocols, setProtocols] = React.useState<SubProtocols>(cur?.protocols ?? 'reality');
  const set = (p: Partial<SubRule>) => setRule((r) => ({ ...r, ...p }));
  const mOpts = React.useMemo(() => machines.map((m) => ({ value: m.id, label: m.kind === 'relay' ? `${m.name}（直出）` : m.name })), [machines]);
  const setInc = React.useCallback((ids: string[]) => setRule((r) => ({ ...r, include_ids: ids })), []);
  const setExc = React.useCallback((ids: string[]) => setRule((r) => ({ ...r, exclude_ids: ids })), []);
  const valid = !!name.trim() && (rule.team_mode !== 'only' || rule.team_ids.length > 0);
  const dirty = cur ? name !== cur.name || JSON.stringify(rule) !== JSON.stringify(cur.rule) || protocols !== cur.protocols : !!name.trim();
  const body = { name: name.trim(), rule: rule.team_mode === 'all' ? { ...rule, team_ids: [] } : rule, protocols };
  const submit = () => {
    if (cur) patch.mutate({ id: cur.id, ...body }, { onSuccess: (s) => { toast.ok(`已保存，现在 ${s.items} 个入口`); onClose(); }, onError: toast.err });
    else create.mutate(body, { onSuccess: (s) => { onClose(); onCreated(s); }, onError: toast.err });
  };
  return (
    <FormDialog size="md" open={open} onOpenChange={(o) => !o && onClose()} title={cur ? `编辑订阅 · ${cur.name}` : '新建客户端订阅'} desc={cur ? '改规则不换链接' : '创建后链接只显示一次'}
      submitLabel={cur ? '保存' : '创建'} busy={create.isPending || patch.isPending} valid={valid} dirty={dirty} onSubmit={submit}>
      <Field label="名称"><Input value={name} onChange={(e) => setName(e.target.value)} placeholder="例如：家里 OpenClash" maxLength={40} autoFocus autoComplete="off" /></Field>
      <Field label="协议" info="AnyTLS 只出在 AnyTLS 在跑的线路机上的入口"><Select value={protocols} onChange={(v) => setProtocols(v as SubProtocols)} options={PROTOS} /></Field>
      <Field label="团队"><Select value={rule.team_mode} onChange={(v) => set({ team_mode: v as SubRule['team_mode'] })} options={MODES} /></Field>
      {rule.team_mode !== 'all' && (
        <div className="scope-list">
          {teams.filter((t) => rule.team_mode === 'only' || !t.spare).map((t) => (
            <label key={t.id} className="scope-row" data-on={rule.team_ids.includes(t.id) ? '1' : undefined}>
              <Checkbox checked={rule.team_ids.includes(t.id)} onCheckedChange={(v) => set({ team_ids: v === true ? [...rule.team_ids, t.id] : rule.team_ids.filter((x) => x !== t.id) })} aria-label={t.name} />
              <span className="scope-name"><b>{t.name}</b>{t.spare && <Chip>备用</Chip>}</span>
            </label>
          ))}
        </div>
      )}
      {rule.team_mode === 'only' && !rule.team_ids.length && <div className="hint is-error">至少选一个团队</div>}
      <label className="check-row" style={{ gap: 10 }}><Switch checked={rule.direct} onCheckedChange={(v) => set({ direct: v })} /><span>含线路机直出入口</span></label>
      <MachinePicks label="另外带上" info="不看团队和直出开关，比如备用团队里的某台" ids={rule.include_ids} onChange={setInc} options={mOpts} />
      <MachinePicks label="一律不带" info="优先于上面所有规则" ids={rule.exclude_ids} onChange={setExc} options={mOpts} />
    </FormDialog>
  );
});
