// 访问规则（策略）：按团队 / 节点设置域名白名单、黑名单或暂停新连接。
// 流程：编辑只存草稿 → 预览（最近 24 小时每个节点会被阻止多少连接）→ 发布（所有草稿一起生效，线路机热加载路由规则）→ 发布记录里可回滚。
import { Field } from '@/components/ui/field';
import * as React from 'react';
import { useSearchParams } from 'react-router-dom';
import { ChevronRight, Eye, Save, ShieldBan, ShieldCheck, Trash2, Undo2 } from 'lucide-react';
import {
  MODE_INFO, modeLabel, useDiscardPolicies, usePolicies, usePolicyPreview, usePolicyVersions, usePublishPolicies, useRollbackPolicy, useRollbackPreview, useSavePolicy,
  type PolicyBody, type PolicyChange, type PolicyMode, type PolicyNode, type PolicyTeam, type PolicyVersion, type ScopePolicy, type VersionRelay, type VersionStatus,
} from '@/lib/policy';
import { MAX_NOTE, diffDomains, domainsToText, parseDomainText, toUnicode } from '@/lib/policyDomains';
import * as F from '@/lib/format';
import { meta } from '@/lib/derive';
import { Button } from '@/components/ui/button';
import { FormField, LabelRow, SearchInput } from '@/components/ui/form';
import { InfoTip } from '@/components/ui/info-tip';
import { Input } from '@/components/ui/input';
import { Textarea } from '@/components/ui/textarea';
import { Segmented } from '@/components/ui/overlays';
import { Trunc, Tooltip, TooltipContent, TooltipTrigger } from '@/components/ui/tooltip';
import { scrollBehavior } from '@/lib/utils';
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogTitle, DialogHeader } from '@/components/ui/dialog';
import { SkeletonRows } from '@/components/app/SkeletonRows';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog } from '@/components/app/dialogs';
import { resetOnOpen, useHeld } from '@/lib/useHeld';
import { AccessViewSwitch } from '@/components/app/AccessViewSwitch';
import { Chip, Empty, ErrorNote, Note, PageHead, Panel, Pill, type Tone } from '@/components/app/primitives';
import { fmtN } from '@/features/AccessBits';
import { ShowAll } from '@/components/app/Pagination';
import '@/styles/policy.css';

const MODE_TONE: Record<PolicyMode, Tone> = { observe: 'neutral', allowlist: 'info', denylist: 'warn', blocked: 'bad', inherit: 'neutral' };
const needsDomains = (m: PolicyMode) => m === 'allowlist' || m === 'denylist';

function ModePill({ mode }: { mode: PolicyMode }) {
  // 黑名单自带盾牌图标（黄色胶囊默认的半圆图标是「降级」的标记，借用会读错）
  if (mode === 'denylist') return <Pill tone="warn" dot="none"><ShieldBan size={12} />{modeLabel(mode)}</Pill>;
  return <Pill tone={MODE_TONE[mode]}>{modeLabel(mode)}</Pill>;
}

type Sel = { scope: 'team' | 'node'; id: string };
const parseSel = (s: string | null): Sel | null => {
  const m = /^(team|node):([\w-]+)$/.exec(s ?? '');
  return m ? { scope: m[1] as Sel['scope'], id: m[2] } : null;
};
const NO_TEAMS: PolicyTeam[] = [];

/** 占比：有但不到 1% 写「<1%」（不写 0%、0.0%），10% 以下留一位小数 */
const pctText = (n: number, total: number) => { const p = (n / total) * 100; return n > 0 && p < 1 ? '<1%' : `${p.toFixed(p < 10 ? 1 : 0)}%`; };

/** 上次选中的范围（scope:id）：命令面板、视图切换回来时地址里没有 ?s= */
let lastSel: string | null = null;

export function PolicyPage() {
  const [sp, setSp] = useSearchParams();
  const q = usePolicies();
  const teams = q.data?.teams ?? NO_TEAMS;
  const draftCount = q.data?.draft_count ?? 0;
  // sel 按值缓存：每次渲染新建对象会让下面按它算的 target、展开分组的 effect 每次都重跑
  const rawSel = sp.get('s') ?? lastSel; // 从别处回来（地址里没有 ?s=）时停在上次看的范围，留着的未保存改动一眼就在
  const firstTeam = teams[0]?.id;
  const sel = React.useMemo<Sel | null>(() => parseSel(rawSel) ?? (firstTeam ? { scope: 'team', id: firstTeam } : null), [rawSel, firstTeam]);
  React.useEffect(() => { if (sel) lastSel = `${sel.scope}:${sel.id}`; }, [sel]);
  const apply = (s: Sel) => setSp((p) => { const n = new URLSearchParams(p); n.set('s', `${s.scope}:${s.id}`); return n; }, { replace: true });
  // 编辑器有未保存的改动时，切换范围会让它重新挂载并丢掉编辑：先拦下来，用页面内对话框确认
  const dirtyRef = React.useRef(false);
  const onDirty = React.useCallback((d: boolean) => { dirtyRef.current = d; }, []);
  const [pending, setPending] = React.useState<Sel | null>(null);
  // 点选触发的切换（含「放弃修改并切换」确认后的）：范围真的换了之后把编辑器滚进视野。单栏（窄屏）时编辑器在长长的范围树下面，点了看不到变化；从地址进来不滚
  const picked = React.useRef(false);
  const colRef = React.useRef<HTMLDivElement>(null);
  const applyPicked = (s: Sel) => { picked.current = true; apply(s); };
  React.useEffect(() => {
    if (!picked.current) return;
    picked.current = false;
    requestAnimationFrame(() => colRef.current?.scrollIntoView({ block: 'nearest', behavior: scrollBehavior() }));
  }, [sel]);
  const select = (s: Sel) => {
    if (sel && sel.scope === s.scope && sel.id === s.id) return;
    if (dirtyRef.current) setPending(s); else applyPicked(s);
  };

  const [openTeams, setOpenTeams] = React.useState<Set<string>>(new Set());
  // 节点多时范围树上方给一个搜索框：按团队名或节点名筛，命中节点的团队自动展开
  const [find, setFind] = React.useState('');
  const nodeCount = teams.reduce((s, t) => s + t.nodes.length, 0);
  const needle = find.trim().toLowerCase();
  const shown = React.useMemo(() => (!needle ? teams.map((t) => ({ t, nodes: t.nodes }))
    : teams.flatMap((t) => {
      const hit = t.name.toLowerCase().includes(needle);
      const nodes = hit ? t.nodes : t.nodes.filter((n) => n.name.toLowerCase().includes(needle));
      return hit || nodes.length ? [{ t, nodes }] : [];
    })), [teams, needle]);
  const [initOpen, setInitOpen] = React.useState(false);
  React.useEffect(() => {
    if (initOpen || !teams.length) return;
    const t = sel?.scope === 'node' ? teams.find((x) => x.nodes.some((n) => n.id === sel.id)) : teams.find((x) => x.id === sel?.id);
    setOpenTeams(new Set(t ? [t.id] : [teams[0].id]));
    setInitOpen(true);
  }, [teams, sel, initOpen]);

  const target = React.useMemo(() => {
    for (const t of teams) {
      if (sel?.scope === 'team' && sel.id === t.id) return { team: t, node: null, sp: t.policy, name: t.name };
      for (const n of t.nodes) if (sel?.scope === 'node' && sel.id === n.id) return { team: t, node: n, sp: n.policy, name: n.name };
    }
    return null;
  }, [teams, sel]);

  const [preview, setPreview] = React.useState(false);
  const [askDiscard, setAskDiscard] = React.useState(false);
  const discard = useDiscardPolicies();

  return (
    <div className="stack">
      <PageHead title="访问监控" note={<>访问规则 <InfoTip text="改动先存为草稿，发布后才会生效" /></>} tools={<AccessViewSwitch value="policy" />} />

      <div className="pol-bar" data-draft={draftCount > 0 ? '1' : undefined}>
        <div className="pol-bar-main">
          {draftCount > 0
            ? <><Chip variant="warn">草稿</Chip><span><b>{draftCount}</b> 份改动未发布</span></>
            : <span className="t-sub row" style={{ gap: 6, flexWrap: 'nowrap' }}><ShieldCheck size={14} />没有未发布的草稿</span>}
        </div>
        <div className="row">
          <Button variant="ghost" disabled={draftCount === 0} onClick={() => setAskDiscard(true)}><Trash2 />丢弃全部草稿</Button>
          <Button disabled={draftCount === 0} onClick={() => setPreview(true)}><Eye />预览并发布</Button>
        </div>
      </div>

      {q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
      <div className="pol-layout">
        <Panel title="范围" count={q.data ? `${teams.length} 个团队` : undefined} className="pol-scope" flush>
          {q.isLoading && <SkeletonRows rows={6} list />}
          {q.data && teams.length === 0 && <Empty text="还没有团队" hint="先去「团队」添加" />}
          {nodeCount > 20 && <div className="pol-find"><SearchInput value={find} onChange={(e) => setFind(e.target.value)} placeholder="搜索团队或节点…" aria-label="搜索团队或节点" /></div>}
          {needle && shown.length === 0 && <div className="pol-none t-muted" style={{ padding: 'var(--sp-8) var(--card-pad)' }}>没有符合的团队或节点</div>}
          <div className="pol-tree" role="tree" aria-label="团队与节点">
            {shown.map(({ t, nodes }) => (
              <TeamRows key={t.id} t={t} nodes={nodes} sel={sel} open={!!needle || openTeams.has(t.id)}
                toggle={() => setOpenTeams((s) => { const n = new Set(s); if (n.has(t.id)) n.delete(t.id); else n.add(t.id); return n; })}
                pick={(s) => { select(s); if (s.scope === 'team') setOpenTeams((o) => new Set(o).add(t.id)); }} />
            ))}
          </div>
        </Panel>
        <div className="pol-col" ref={colRef}>
          {target
            ? <Editor key={`${sel!.scope}:${sel!.id}`} scope={sel!.scope} name={target.name} sp={target.sp} team={target.team} node={target.node} onDirty={onDirty} />
            : <Panel title="编辑"><Empty icon={<ShieldCheck size={22} />} text="在左侧选择团队或节点" /></Panel>}
        </div>
      </div>

      <VersionsPanel />

      <PreviewDialog open={preview} onOpenChange={setPreview} />
      <ConfirmDialog open={pending != null} onOpenChange={(o) => { if (!o) setPending(null); }} danger confirmLabel="放弃修改并切换" title="放弃未保存的修改？"
        description={`「${target?.name ?? '当前范围'}」的修改会丢失`}
        onConfirm={() => { if (pending) { if (sel) unsaved.delete(`${sel.scope}:${sel.id}`); dirtyRef.current = false; applyPicked(pending); } }} />
      <ConfirmDialog open={askDiscard} onOpenChange={setAskDiscard} danger confirmLabel="丢弃" title={`丢弃全部 ${draftCount} 份草稿？`}
        description="已生效的规则不受影响"
        onConfirm={async () => {
          const h = q.data?.draft_hash;
          if (!h) throw new Error('没有可丢弃的草稿');
          const r = await discard.mutateAsync(h);
          toast.ok(`已丢弃 ${r.discarded} 份草稿`);
        }} />
    </div>
  );
}

/* ================================================================ 左栏 */
/** nodes：要列出的节点（搜索时只列命中的）。折叠时写出有几个节点不跟随团队（例外） */
function TeamRows({ t, nodes, sel, open, toggle, pick }: { t: PolicyTeam; nodes: PolicyNode[]; sel: Sel | null; open: boolean; toggle: () => void; pick: (s: Sel) => void }) {
  const here = sel?.scope === 'team' && sel.id === t.id;
  const drafts = (t.policy.draft ? 1 : 0) + t.nodes.filter((n) => n.policy.draft).length;
  const own = t.nodes.filter((n) => n.effective.source === 'node').length;
  return (
    <>
      <div className="pol-row" role="treeitem" aria-expanded={open} aria-selected={here}>
        <button type="button" className="pol-chev" aria-label={`${open ? '收起' : '展开'}${t.name}`} data-open={open ? '1' : undefined} onClick={toggle}><ChevronRight size={14} /></button>
        <button type="button" className="pol-item" aria-current={here} onClick={() => pick({ scope: 'team', id: t.id })}>
          <span className="pol-name"><Trunc className="strong">{t.name}</Trunc><span className="pol-sub">{t.nodes.length} 个节点{!open && own > 0 ? ` · ${own} 个例外` : ''}{drafts > 0 && !t.policy.draft ? ` · ${drafts} 份草稿` : ''}</span></span>
          {t.policy.draft && <Tooltip><TooltipTrigger render={<span />}><Chip variant="warn">草稿</Chip></TooltipTrigger><TooltipContent>{`草稿：${modeLabel(t.policy.draft.mode)}`}</TooltipContent></Tooltip>}
          <ModePill mode={t.policy.mode} />
        </button>
      </div>
      {open && nodes.map((n) => <NodeRow key={n.id} n={n} here={sel?.scope === 'node' && sel.id === n.id} pick={pick} />)}
    </>
  );
}

function NodeRow({ n, here, pick }: { n: PolicyNode; here: boolean; pick: (s: Sel) => void }) {
  return (
    <div className="pol-row pol-row-node" role="treeitem" aria-selected={here}>
      <button type="button" className="pol-item" aria-current={here} onClick={() => pick({ scope: 'node', id: n.id })}>
        <span className="pol-name"><Trunc className="strong">{n.name}</Trunc>{!n.has_entry && <span className="pol-sub">还没有入口</span>}</span>
        {n.policy.draft && <Tooltip><TooltipTrigger render={<span />}><Chip variant="warn">草稿</Chip></TooltipTrigger><TooltipContent>{`草稿：${modeLabel(n.policy.draft.mode)}`}</TooltipContent></Tooltip>}
        {/* 跟随团队的不挂胶囊（团队行上有）：只标和团队设置不同的节点 */}
        {n.effective.source === 'node' && <ModePill mode={n.effective.mode} />}
      </button>
    </div>
  );
}

/* ================================================================ 右栏：编辑器 */
const bodyOf = (p: ScopePolicy): PolicyBody => p.draft ?? { mode: p.mode, domains: p.domains, note: p.note };
const curOf = (p: ScopePolicy): PolicyBody => ({ mode: p.mode, domains: p.domains, note: p.note });
const sameBody = (a: PolicyBody, b: PolicyBody) => a.mode === b.mode && a.note === b.note && a.domains.length === b.domains.length && a.domains.every((d, i) => d === b.domains[i]);

/** 编辑器里还没保存成草稿的改动，按范围留在模块里：离开访问规则页（或切到别的范围后又切回来）再进来还在；保存或改回原样即清掉。
 *  只在这个标签页的内存里：刷新、关标签页前提醒（在任何页面都提醒，离开访问规则页后缓存也还在） */
const unsaved = new Map<string, { mode: PolicyMode; text: string; note: string; synced: { revision: number; body: PolicyBody } }>();
window.addEventListener('beforeunload', (e) => { if (unsaved.size) { e.preventDefault(); e.returnValue = ''; } });

function Editor({ scope, name, sp, team, node, onDirty }: { scope: 'team' | 'node'; name: string; sp: ScopePolicy; team: PolicyTeam; node: PolicyNode | null; onDirty: (d: boolean) => void }) {
  const save = useSavePolicy();
  // 表单初值 = 草稿（没有草稿则生效值）；synced 记录它对应的服务端 revision 与内容，用来判断有没有本地改动、服务端有没有被别人改过
  const init = bodyOf(sp);
  const key = `${scope}:${sp.id}`;
  const [buf] = React.useState(() => unsaved.get(key));
  const [mode, setMode] = React.useState<PolicyMode>(buf?.mode ?? init.mode);
  const [text, setText] = React.useState(buf?.text ?? domainsToText(init.domains));
  const [note, setNote] = React.useState(buf?.note ?? init.note);
  const [synced, setSynced] = React.useState(buf?.synced ?? { revision: sp.revision, body: init });

  const parsed = React.useMemo(() => parseDomainText(text), [text]);
  // 「请填写域名」等到域名框失焦或点了保存再提示：刚切到白名单时还没来得及填
  const [touched, setTouched] = React.useState(false);
  // 光标所在的那一行还在输入：它的问题等换行或失焦后再报（输入「exa」时不整框标红「必须包含点」）
  const [caret, setCaret] = React.useState<number | null>(null);
  const trackCaret = (el: HTMLTextAreaElement) => setCaret(el.value.slice(0, el.selectionStart).split('\n').length);
  const uses = needsDomains(mode);
  // 不用域名的模式（仅观察、暂停新连接、跟随团队）提交空域名：隐藏着的旧域名不算改动（服务端同样丢掉）
  const form: PolicyBody = { mode, domains: uses ? parsed.domains : [], note: note.trim() };
  const dirty = !sameBody(form, synced.body);
  const stale = sp.revision !== synced.revision;
  React.useEffect(() => { onDirty(dirty); return () => onDirty(false); }, [dirty, onDirty]);
  React.useEffect(() => { if (dirty) unsaved.set(key, { mode, text, note, synced }); else unsaved.delete(key); }, [key, dirty, mode, text, note, synced]);

  const load = (p: ScopePolicy) => {
    const b = bodyOf(p);
    setMode(b.mode); setText(domainsToText(b.domains)); setNote(b.note); setSynced({ revision: p.revision, body: b });
  };
  // 服务端的 revision 变了而本地没有改动：直接跟上；有改动则提示冲突，由用户决定
  React.useEffect(() => {
    if (sp.revision !== synced.revision && !dirty) load(sp);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sp.revision]);

  const noDomains = uses && parsed.domains.length === 0;
  const invalid = (uses && !!parsed.overall) || note.length > MAX_NOTE;
  // 没填域名、正在输入的那行有问题（还没亮出来）时保存按钮仍可点：点了才提示「请填写域名」、亮出那一行的问题
  const canSave = dirty && !invalid;
  const onSave = () => { if (noDomains) setTouched(true); else if (uses && parsed.issues.length) setCaret(null); else void submit(form); };

  const req = (body: PolicyBody) => ({ scope, id: sp.id, ...body, expected_revision: synced.revision });
  const onSaved = (r: Awaited<ReturnType<typeof save.mutateAsync>>) => {
    const b = r.policy.draft ?? curOf(r.policy);
    setMode(b.mode); setText(domainsToText(b.domains)); setNote(b.note); setSynced({ revision: r.policy.revision, body: b });
    toast.ok(r.policy.draft ? '草稿已保存' : '草稿已清除（与生效值相同）');
  };
  // 保存还在路上就离开了页面：单次回调不会再触发，用 Promise 保证清掉留着的改动（否则回来是旧 revision、显示假冲突）
  const submit = (body: PolicyBody) => save.mutateAsync(req(body)).then((r) => { unsaved.delete(key); onSaved(r); }, toast.err);
  const [askDrop, setAskDrop] = React.useState(false);

  const cur = curOf(sp);
  // 节点「跟随团队」时对比里要说明团队当前是什么
  const teamCur = curOf(team.policy);
  const lines = text.split('\n');
  const showIssues = uses ? parsed.issues.filter((i) => i.line !== caret) : [];
  const badLines = new Set(showIssues.map((i) => i.line));
  // 问题已经亮出来（标红的行、「请填写域名」）时保存置灰、悬停说明为什么（点了也不会有新反馈）
  const blocked = !canSave ? undefined : showIssues.length ? '先改正标红的行' : noDomains && touched ? '请先填写域名' : undefined;
  const modes: PolicyMode[] = scope === 'node' ? ['inherit', 'observe', 'allowlist', 'denylist', 'blocked'] : ['observe', 'allowlist', 'denylist', 'blocked'];

  return (
    <Panel className="pol-edit" title={name} count={scope === 'team' ? '团队规则' : '节点规则'}
      sub={scope === 'team' ? <InfoTip text="团队内选了「跟随团队」的节点都会使用这里的设置" /> : `当前生效：${modeLabel(node!.effective.mode)}${node!.effective.source === 'team' ? `（来自 ${team.name}）` : ''}`}>
      <div className="pol-form">
        {stale && (
          <Note tone="warn">
            草稿已被别人改过 <InfoTip text="直接保存会提示冲突。载入最新的，会丢掉你正在改的内容" />
            <Button size="sm" variant="outline" style={{ marginLeft: 8 }} onClick={() => load(sp)}>载入最新</Button>
          </Note>
        )}

        <Field>
          <LabelRow info={MODE_INFO[mode]}>模式</LabelRow>
          <Segmented label="模式" value={mode} onChange={setMode} items={modes.map((m) => ({ value: m, label: modeLabel(m) }))} />
          {mode === 'inherit' && <div className="hint">团队现在是「{modeLabel(teamCur.mode)}」{needsDomains(teamCur.mode) ? `（${teamCur.domains.length} 个域名）` : ''}</div>}
        </Field>

        {/* 只有白名单 / 黑名单用域名列表：别的模式不显示（已填的内容留着，切回来仍在） */}
        {uses && <Field>
          <LabelRow info={"用逗号、空格或换行分隔\nexample.com 会连同所有子域名一起匹配，=example.com 只匹配它自己"}>域名<span className="pol-count"> · {fmtN(parsed.domains.length)} 个有效</span></LabelRow>
          <div className="pol-ed" data-bad={showIssues.length > 0 || (noDomains && touched) ? '1' : undefined}>
            <Gutter lines={lines.length} bad={badLines} />
            <Textarea className="pol-ta h-[260px] min-h-[260px] min-w-0 flex-1 resize-none overflow-auto rounded-none border-0 bg-transparent px-2.5 py-2 font-mono text-label leading-5 whitespace-pre shadow-none focus-visible:border-0 focus-visible:ring-0" value={text} wrap="off" spellCheck={false} autoComplete="off" aria-label="域名列表" aria-invalid={showIssues.length > 0 ? true : undefined}
              placeholder={'example.com\n=exact.example.org'} onBlur={() => { setTouched(true); setCaret(null); }}
              onSelect={(e) => trackCaret(e.currentTarget)}
              onChange={(e) => { setText(e.target.value); trackCaret(e.currentTarget); }} onScroll={(e) => { const g = e.currentTarget.previousElementSibling as HTMLElement | null; if (g) g.scrollTop = e.currentTarget.scrollTop; }} />
          </div>
          {noDomains && touched && showIssues.length === 0 && <div className="hint is-error">请填写域名</div>}
          {parsed.overall && <div className="hint is-error">{parsed.overall}</div>}
          {showIssues.length > 0 && (
            <ul className="pol-issues" role="alert">
              {showIssues.slice(0, 6).map((i, k) => <li key={k}><b>第 {i.line} 行</b>{i.error}</li>)}
              {showIssues.length > 6 && <li className="t-sub">还有 {showIssues.length - 6} 处问题</li>}
            </ul>
          )}
        </Field>}

        <FormField label="备注" hint={`${note.length}/${MAX_NOTE}`} error={note.length > MAX_NOTE ? `备注最多 ${MAX_NOTE} 个字符` : undefined}>
          <Input value={note} onChange={(e) => setNote(e.target.value)} placeholder="为什么这样设置（选填）" autoComplete="off" />
        </FormField>

        <Compare cur={cur} next={form} scope={scope} teamCur={teamCur} teamName={team.name} />

        <div className="pol-actions">
          <Button disabled={!canSave} aria-disabled={!!blocked || undefined} data-tip={blocked} loading={save.isPending} onClick={() => { if (!blocked) onSave(); }}><Save />保存草稿</Button>
          <Button variant="ghost" disabled={!dirty || save.isPending} onClick={() => load(sp)}><Undo2 />还原编辑</Button>
          {sp.draft && <Button variant="destructive-outline" disabled={save.isPending} onClick={() => setAskDrop(true)}><Trash2 />放弃这份草稿</Button>}
          <span className="pol-meta t-sub">{dirty ? <span className="pol-dirty">有未保存的修改</span> : sp.updated_at ? <span data-tip={F.dtFull(sp.updated_at)}>最近修改 {F.dt(sp.updated_at)}{sp.updated_by ? ` · ${F.actor(sp.updated_by)}` : ''}</span> : '还没有修改过'}</span>
        </div>
      </div>
      <ConfirmDialog open={askDrop} onOpenChange={setAskDrop} title={`放弃草稿 · ${name}`} confirmLabel="放弃草稿" danger
        description={`草稿里的改动会丢失${dirty ? '，未保存的编辑也会丢' : ''}`}
        onConfirm={() => save.mutateAsync(req(cur)).then(onSaved)} />
    </Panel>
  );
}

/** 行号栏：非法行标红；与文本框同步滚动（文本框不换行，一行对一行） */
function Gutter({ lines, bad }: { lines: number; bad: Set<number> }) {
  return <div className="pol-gutter" aria-hidden>{Array.from({ length: Math.max(lines, 12) }, (_, i) => <div key={i} data-bad={bad.has(i + 1) ? '1' : undefined}>{i < lines ? i + 1 : ''}</div>)}</div>;
}

/** 生效值 vs 草稿：模式、域名（新增绿色、删除红色划线）、备注 */
function Compare({ cur, next, scope, teamCur, teamName }: { cur: PolicyBody; next: PolicyBody; scope: 'team' | 'node'; teamCur: PolicyBody; teamName: string }) {
  const same = sameBody(cur, next);
  const d = diffDomains(needsDomains(cur.mode) ? cur.domains : [], needsDomains(next.mode) ? next.domains : []);
  const side = (b: PolicyBody, mark: 'add' | 'del', set: Set<string>) => (
    <div className="pol-side">
      <div className="pol-side-head">
        <ModePill mode={b.mode} />
        {b.mode === 'inherit' && scope === 'node' && <span className="t-sub">{teamName}：{modeLabel(teamCur.mode)}</span>}
        {needsDomains(b.mode) && <span className="t-sub">{fmtN(b.domains.length)} 个域名</span>}
      </div>
      {needsDomains(b.mode) ? <Chips domains={b.domains} mark={mark} marked={set} /> : b.mode === 'inherit' && needsDomains(teamCur.mode) ? <Chips domains={teamCur.domains} /> : <div className="t-muted pol-none">没有域名规则</div>}
      {b.note && <div className="pol-note t-sub">备注：{b.note}</div>}
    </div>
  );
  return (
    <section className="pol-cmp" aria-label="生效值与草稿对比">
      <div className="pol-cmp-head"><span>当前生效</span><span>{same ? '编辑结果（与生效值相同）' : '编辑结果（保存为草稿）'}</span></div>
      <div className="pol-cmp-body">
        {side(cur, 'del', new Set(d.removed))}
        {side(next, 'add', new Set(d.added))}
      </div>
      {!same && (d.added.length > 0 || d.removed.length > 0) && <div className="pol-cmp-foot t-sub">新增 <b className="pol-add">{d.added.length}</b> 个，移除 <b className="pol-del">{d.removed.length}</b> 个</div>}
    </section>
  );
}

const CHIP_LIMIT = 30;
function Chips({ domains, mark, marked }: { domains: string[]; mark?: 'add' | 'del'; marked?: Set<string> }) {
  const list = domains.slice(0, CHIP_LIMIT);
  return (
    <div className="pol-chips">
      {list.map((d) => <DomainTag key={d} d={d} mark={marked?.has(d) ? mark : undefined} />)}
      {domains.length > CHIP_LIMIT && <span className="pol-more">还有 {fmtN(domains.length - CHIP_LIMIT)} 个</span>}
      {domains.length === 0 && <span className="t-muted pol-none">没有域名</span>}
    </div>
  );
}
/** 国际化域名写原文，xn-- 形式放在括号里：「中文.中国（xn--fiq228c.xn--fiqs8s）」 */
function DomainTag({ d, mark }: { d: string; mark?: 'add' | 'del' }) {
  const exact = d.startsWith('=');
  const ascii = exact ? d.slice(1) : d;
  const uni = toUnicode(ascii);
  return <span className="pol-tag" data-mark={mark}>{uni === ascii ? ascii : <>{uni}<small>{ascii}</small></>}{exact && <small>精确</small>}</span>;
}

/* ================================================================ 预览与发布 */
/** 每次打开重新挂载（resetOnOpen），只显示这次打开后算出的预览：缓存里还留着上一次的（关着时对话框也挂着，观察者没走，gcTime 0 回收不了） */
const PreviewDialog = resetOnOpen(function PreviewDialog({ open, onOpenChange }: { open: boolean; onOpenChange: (o: boolean) => void }) {
  const q = usePolicyPreview(open);
  const publish = usePublishPolicies();
  const [ask, setAsk] = React.useState(false);
  const d = q.isFetchedAfterMount && q.isSuccess ? q.data : undefined;
  // 这次发布没改到的节点（比如节点自己的规则）先收起来，排在改到的后面
  const [showSame, setShowSame] = React.useState(false);
  const changed = d?.nodes.filter((n) => n.changed) ?? [];
  const same = d?.nodes.filter((n) => !n.changed) ?? [];
  const totalBlock = d?.nodes.reduce((s, n) => s + n.would_block, 0) ?? 0;
  const total = d?.nodes.reduce((s, n) => s + n.total_conns, 0) ?? 0;
  const empty = !!d && d.draft_count === 0;
  return (
    <>
      <Dialog open={open} onOpenChange={onOpenChange}>
        <DialogContent size="lg">
          <DialogHeader>
            <DialogTitle>预览：发布后会怎样</DialogTitle>
            <DialogDescription>{d ? <>根据最近 {d.window_hours} 小时的访问估算 <InfoTip text="这只是估算，实际以发布后为准" /></> : '正在估算…'}</DialogDescription>
          </DialogHeader>
          {!q.isFetchedAfterMount && <SkeletonRows rows={4} list />}
          {q.isFetchedAfterMount && q.isError && <ErrorNote error={q.error} onRetry={() => void q.refetch()} />}
          {empty && <Empty icon={<ShieldCheck size={22} />} text="没有未发布的草稿" />}
          {d && !empty && (
            <div className="pol-pv">
              <div className="pol-sum">
                <div><span>草稿</span><b>{d.draft_count}</b><small>份</small></div>
                <div><span>更新线路机</span><b>{d.affected_relays}</b><small>台</small></div>
                <div><span>会被阻止的连接</span><b>{fmtN(totalBlock)}</b><small>{total > 0 ? `占 ${pctText(totalBlock, total)}` : '近 24 小时无记录'}</small></div>
              </div>
              <ChangeList changes={d.changes} />
              {d.nodes.length === 0
                ? <Note tone="info">不会多阻止连接{d.affected_relays === 0 && <> <InfoTip text="也不需要改线路机上的规则" /></>}</Note>
                : (
                  <div className="table-wrap is-boxed is-capped">
                    <table className="dt" style={{ minWidth: 620 }}>
                      <colgroup><col style={{ width: 120 }} /><col style={{ width: 96 }} /><col style={{ width: 84 }} /><col style={{ width: 96 }} /><col /></colgroup>
                      <thead><tr><th>节点</th><th>模式</th><th className="r">24 小时连接</th><th className="r">将阻止</th><th>被阻止最多的域名</th></tr></thead>
                      <tbody>
                        {/* 改到的团队里还没有节点（或节点都有自己的规则）：只有这次没变的别处节点，先写明没有节点受影响 */}
                        {changed.length === 0 && <tr><td colSpan={5} className="t-muted">没有节点的规则因这次发布改变</td></tr>}
                        {[...changed, ...(showSame ? same : [])].map((n) => (
                          <tr key={n.node_id} className={n.changed ? undefined : 'is-retired'}>
                            <td><Trunc className="cell-main strong">{n.name}</Trunc><span className="sub">{!n.changed ? '这次没变' : n.source === 'team' ? '跟随团队' : n.source === 'node' ? '节点自己的' : ''}</span></td>
                            <td><ModePill mode={n.mode} /></td>
                            <td className="r">{fmtN(n.total_conns)}</td>
                            <td className="r">{n.would_block > 0 ? <><b className="strong">{fmtN(n.would_block)}</b><span className="sub">{n.total_conns > 0 ? pctText(n.would_block, n.total_conns) : ''}</span></> : <span className="t-muted">0</span>}</td>
                            <td>
                              {n.domains.length === 0 ? <span className="t-muted">—</span> : (
                                <Tooltip>
                                  <TooltipTrigger render={<span className="pol-hosts" tabIndex={0} />}>
                                    {n.domains.slice(0, 1).map((x) => <span key={x.host} className="pol-tag">{x.host}<small>{fmtN(x.conns)}</small></span>)}
                                    {n.domains.length > 1 && <span className="pol-more">+{n.domains.length - 1}</span>}
                                  </TooltipTrigger>
                                  <TooltipContent side="left"><div className="pol-tip">{n.domains.map((x) => <div key={x.host}><span className="mono">{x.host}</span> <b>{fmtN(x.conns)}</b></div>)}</div></TooltipContent>
                                </Tooltip>
                              )}
                            </td>
                          </tr>
                        ))}
                      </tbody>
                    </table>
                    {same.length > 0 && <ShowAll total={changed.length + same.length} cap={changed.length} open={showSame} onToggle={() => setShowSame((v) => !v)} unit="个有规则的节点（含各团队这次没变的）" />}
                  </div>
                )}
            </div>
          )}
          <DialogFooter>
            <Button variant="outline" onClick={() => onOpenChange(false)}>关闭</Button>
            <Button disabled={!d || empty || q.isFetching} onClick={() => setAsk(true)}>发布…</Button>
          </DialogFooter>
        </DialogContent>
      </Dialog>
      <ConfirmDialog open={ask} onOpenChange={setAsk} confirmLabel="确认发布" title={`发布 ${d?.draft_count ?? 0} 份草稿？`}
        description={<>立即生效，可以回滚 <InfoTip text={`${d?.affected_relays ? `${d.affected_relays} 台线路机会直接载入新规则，不重启，已有连接不受影响` : '不需要线路机做任何事'}\n之后可以在「发布记录」里回滚`} /></>}
        onConfirm={async () => {
          if (!d?.draft_hash) throw new Error('草稿已变化，请重新预览');
          const v = await publish.mutateAsync(d.draft_hash);
          toast.ok(v.relays.length ? `已发布为版本 ${v.seq}，正在应用到 ${v.relays.length} 台线路机` : `已发布为版本 ${v.seq}`);
          onOpenChange(false);
        }} />
    </>
  );
});

function ChangeList({ changes }: { changes: PolicyChange[] }) {
  if (!changes.length) return null;
  const sum = (c: PolicyChange['from']) => `${modeLabel(c.mode)}${needsDomains(c.mode) ? ` · ${c.domains} 个域名` : ''}`;
  return (
    <ul className="pol-changes">
      {changes.map((c) => <li key={`${c.scope}:${c.id}`}><span className="pill pill-sm">{c.scope === 'team' ? '团队' : '节点'}</span><b>{c.name}</b><span className="t-sub">{sum(c.from)}</span><span className="pol-arrow">→</span><span>{sum(c.to)}</span></li>)}
    </ul>
  );
}

/* ================================================================ 发布记录 */
/** 同步状态的色调（样式）；中文名在 /meta 的 policy_sync_states */
const STATUS: Record<VersionStatus | 'gone', { tone: Tone; dot?: 'pulse' }> = {
  applied: { tone: 'ok' }, applying: { tone: 'info', dot: 'pulse' }, failed: { tone: 'bad' }, gone: { tone: 'neutral' },
};
const StatusPill = ({ s }: { s: VersionStatus | 'gone' }) => <Pill tone={STATUS[s].tone} dot={STATUS[s].dot}>{meta().policy_sync_states[s] ?? s}</Pill>;

function VersionsPanel() {
  const q = usePolicyVersions();
  const rollback = useRollbackPolicy();
  const [ask, setAsk] = React.useState<PolicyVersion | null>(null);
  const askHeld = useHeld(ask?.id, ask ?? undefined); // 关闭动画期间标题里的版本号不变空
  const pv = useRollbackPreview(ask?.id ?? null);
  const list = q.data ?? [];
  return (
    <>
      <Panel title="发布记录" count={q.data ? `最近 ${list.length} 个版本` : undefined} sub={<InfoTip text="每次发布都保存完整的规则。回滚就是把旧版本再发布一次" />} flush>
        {q.isError && <div style={{ padding: 'var(--sp-12) var(--card-pad)' }}><ErrorNote error={q.error} onRetry={() => void q.refetch()} /></div>}
        {q.isLoading ? <SkeletonRows rows={3} head /> : list.length === 0 && !q.isError ? (
          <Empty icon={<ShieldCheck size={22} />} text="还没有发布过访问规则" />
        ) : (
          <div className="table-wrap">
            <table className="dt" style={{ minWidth: 900 }}>
              <colgroup><col style={{ width: 116 }} /><col style={{ width: 150 }} /><col /><col style={{ width: 240 }} /><col style={{ width: 110 }} /></colgroup>
              <thead><tr><th>版本</th><th>时间</th><th>变更</th><th>状态</th><th /></tr></thead>
              <tbody>
                {list.map((v) => (
                  <tr key={v.id}>
                    {/* 「当前」只标最新一版；更早的、内容和当前一样的写「与当前相同」（不能回滚到它） */}
                    <td><b className="strong">v{v.seq}</b>{v.is_latest ? <span className="pill pill-sm pol-cur" data-tone="accent">当前</span> : v.is_current && <span className="pill pill-sm pol-cur">与当前相同</span>}<span className="sub">{v.kind === 'rollback' ? `回滚到 v${v.rollback_of_seq ?? '?'}` : v.kind === 'replace' ? '替换节点' : '发布'}</span></td>
                    <td data-tip={`${F.dtFull(v.created_at)}\n${F.actor(v.actor)}`}>{F.dt(v.created_at)}<span className="sub ellipsis">{F.actor(v.actor)}</span></td>
                    <td><VersionChanges changes={v.changes} /></td>
                    <td><VersionApply v={v} /></td>
                    <td className="r">{v.status === 'applied' && v.rollbackable && !v.is_latest && <Button variant="outline" size="sm" aria-label={`回滚到版本 v${v.seq}`} data-tip={`回滚到版本 v${v.seq}`} onClick={() => setAsk(v)}><Undo2 size={13} />回滚</Button>}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>
      <ConfirmDialog open={!!ask} onOpenChange={(o) => !o && setAsk(null)} confirmLabel="回滚" confirmDisabled={!pv.data} title={`回滚到版本 v${askHeld?.seq ?? ''}？`}
        description={<>作为新版本发布，草稿不受影响 <InfoTip text="当前规则会整体换成这个版本的内容，并作为新版本发布" /></>}
        onConfirm={async () => { const v = await rollback.mutateAsync(ask!.id); toast.ok(`已回滚，新版本 v${v.seq}`); }}>
        {/* 回滚实际会改什么（当前 → 这个版本）：「模拟团队A：黑名单 → 仅观察」 */}
        {pv.isError ? <ErrorNote error={pv.error} onRetry={() => void pv.refetch()} />
          : <ul className="pol-changes">{pv.data ? pv.data.changes.map((c) => <li key={`${c.scope}:${c.id}`}>{changeText(c)}</li>) : <li className="t-sub">正在比较…</li>}</ul>}
      </ConfirmDialog>
    </>
  );
}

/** 状态：整体状态 + 线路机（全部已应用只写台数，悬停看各台；有没应用上的列出那几台） */
function VersionApply({ v }: { v: PolicyVersion }) {
  if (v.relays.length === 0) return <span className="t-sub">无需应用</span>;
  const tip = (r: VersionRelay) => (r.error ? `${r.name ?? '线路机'}：${r.error}` : `${r.name ?? '线路机'}：${meta().policy_sync_states[r.status] ?? r.status}`);
  const pending = v.relays.filter((r) => r.status !== 'applied');
  return (
    <span className="pol-apply">
      <StatusPill s={v.status} />
      {pending.length === 0
        ? <span className="t-sub" data-tip={v.relays.map(tip).join('\n')}>{v.relays.length} 台线路机</span>
        : <span className="pol-relays">{pending.map((r) => <span key={r.relay_id} className="pol-relay" tabIndex={0} data-tip={tip(r)}>{r.name ?? '线路机'}<small>{meta().policy_sync_states[r.status] ?? r.status}</small></span>)}</span>}
    </span>
  );
}

/** 一处变更的说明（同服务端审计摘要）：模式没变时写域名怎么变、备注改没改；变了写模式（新模式用域名时带上个数） */
function changeText(c: PolicyChange): string {
  const n = (k: number) => `${k} 个域名`;
  if (c.from.mode === c.to.mode) {
    const parts = [c.domains_changed && (c.from.domains === c.to.domains ? `换了域名（${n(c.to.domains)}）` : `域名 ${c.from.domains} → ${c.to.domains} 个`), c.note_changed && '改了备注'].filter(Boolean);
    return `${c.name}：${modeLabel(c.to.mode)}${parts.length ? ` · ${parts.join('，')}` : ''}`;
  }
  return `${c.name}：${modeLabel(c.from.mode)} → ${modeLabel(c.to.mode)}${needsDomains(c.to.mode) ? `（${n(c.to.domains)}）` : ''}`;
}

function VersionChanges({ changes }: { changes: PolicyChange[] }) {
  if (!changes.length) return <span className="t-sub">只改了备注</span>;
  const one = changeText;
  const full = changes.map(one).join('\n');
  // 完整内容只给一个提示：两项以内悬停整格看全文（data-tip，换行分项），多于两项用 Tooltip 里的列表；每行自己只省略、不再各挂截断提示
  const cell = (
    <div className="pol-vchg" data-tip={changes.length > 2 ? undefined : full}>
      {changes.slice(0, 2).map((c) => <span key={`${c.scope}:${c.id}`} className="cell-main">{one(c)}</span>)}
      {changes.length > 2 && <span className="sub">还有 {changes.length - 2} 项</span>}
    </div>
  );
  if (changes.length <= 2) return cell;
  return (
    <Tooltip>
      <TooltipTrigger render={cell} />
      <TooltipContent><div className="pol-tip">{changes.map((c) => <div key={`${c.scope}:${c.id}`}>{one(c)}</div>)}</div></TooltipContent>
    </Tooltip>
  );
}
