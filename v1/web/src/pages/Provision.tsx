// 上架：单台表单 + 批量可编辑网格（可从表格软件粘贴）。两步：1 检查（不落库，显示每行错误与分配到的名称/端口）→ 2 开始（唯一一次确认）。
// VPS 节点可以「在已有机器上添加 IP」：同机 IP 节点（host_id），SSH、agent、供应商沿用宿主机的；抽屉里宿主机的「添加 IP 节点」跳来时带 ?host=<id> 预填。
import * as React from 'react';
import { Link, useSearchParams } from 'react-router-dom';
import { Check, CheckCircle2, ChevronDown, ClipboardPaste, Info, KeyRound, ListChecks, Plus, Trash2, TriangleAlert } from 'lucide-react';
import { useHostChoices, usePlans, useProviders, useProvision, useProvisionCheck, useRelayChoices, useTeams, type HostChoice } from '@/lib/queries';
import type { Kind, ProvisionCheckItem } from '@/lib/types';
import { KIND_ORDER } from '@/lib/sort';
import { prefixScore } from '@/lib/match';
import { inheritSummary, kindLabel, meta } from '@/lib/derive';
import * as F from '@/lib/format';
import { Button, IconButton } from '@/components/ui/button';
import { Field, InfoTip, Input, Switch } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { DateField } from '@/components/ui/date-field';
import { Popover, PopoverContent, PopoverTrigger, Segmented } from '@/components/ui/overlays';
import { Command, CommandEmpty, CommandInput, CommandItem, CommandList } from '@/components/ui/command';
import { ConfirmDialog } from '@/components/app/dialogs';
import { errMessage, toast, useToastLift, type ToastTone } from '@/components/ui/toast';
import { copyText } from '@/lib/clipboard';
import { PageHead, Panel, Pill } from '@/components/app/primitives';
import { COLS, PASTE_COLS, UNUSED_LABEL, applies, headerCols, ipIssue, isBlank, isIpNode, localErrors, localIssues, newRow, parseTsv, pasteLine, portIssue, serverField, toItem, type Col, type Ctx, type Row } from '@/features/provision-model';
import { CountrySelect } from '@/components/app/CountrySelect';

// 上架页懒加载、在外壳等到 /meta 之后才求值，类型名、默认端口可以在模块里取
const KIND_OPTS = KIND_ORDER.map((value) => ({ value, label: kindLabel(value) }));

const NONE: never[] = [];
/** 批量表格的粘贴与填写说明（面板标题旁的「(i)」） */
const BATCH_TIP = '· 可从表格软件整块粘贴，带表头按列名对上\n· 编号留空自动接在末尾\n· 灰格不适用，粘贴时跳过\n· 选择列粘名称；国家粘代码或中文名，空 = 按 IP\n· 填宿主机 = 同机 IP 节点，登录、供应商沿用宿主机';
/** 检查结果的通知只留最新一条，内容一改就收起 */
const CHECK_TOAST = 'provision-check';
const checkToast = (tone: ToastTone, text: string) => toast.show({ tone, text, key: CHECK_TOAST });
const NONE_RELAYS: { id: string; name: string }[] = [];
const NONE_HOSTS: HostChoice[] = [];
/** 「复制中控公钥」：供应商面板（或机器 root 的 authorized_keys）录入它之后，上架不用填 root 密码。中控读不到公钥时不显示入口 */
const PUBKEY_TIP = '供应商面板录入这把公钥后可以不填密码';
const copyPubkey = () => { const k = meta().ssh_public_key; if (k) void copyText(k, '中控公钥'); };
function PubkeyButton() {
  return <Button variant="link" onClick={copyPubkey}><KeyRound size={13} />复制中控公钥</Button>;
}
/** 检查结果里的提醒条数（不影响通过） */
const warnCount = (rs: ProvisionCheckItem[]) => rs.reduce((n, r) => n + (r.warnings?.length ?? 0), 0);

/** 离开上架页再回来不丢：表格（含登录密码，只在这个标签页的内存里，刷新、退出登录即无）、模式与检查结果留在模块里；上架成功后清掉 */
interface Draft { mode: 'single' | 'batch'; rows: Row[]; results: ProvisionCheckItem[] | null; localErrs: string[][] | null; checked: string[]; sig: string }
let draft: Draft | null = null;
// 草稿里有内容时，关标签页、刷新前浏览器先问一句——在任何页面都提醒（草稿在模块里，离开上架页后也还在）
window.addEventListener('beforeunload', (e) => { if (draft?.rows.some((r) => !isBlank(r))) { e.preventDefault(); e.returnValue = ''; } });

export function ProvisionPage() {
  const teams = useTeams().data ?? NONE;
  const providers = useProviders().data ?? NONE;
  const plans = usePlans().data ?? NONE;
  // 只能挂在已上线的线路机上。只取 id/名称，引用在内容不变时保持稳定，ctx 不随每次轮询变化（GridCell 的 memo 才有效）
  const relays = useRelayChoices().data ?? NONE_RELAYS;

  const [sp, setSp] = useSearchParams();
  // 单台 / 批量记在 URL（?mode=batch），刷新、分享都停在原来的模式；不带参数时沿用草稿里的
  const mode: 'single' | 'batch' = sp.get('mode') === 'batch' || (!sp.has('mode') && draft?.mode === 'batch') ? 'batch' : 'single';
  const setMode = (m: 'single' | 'batch') => setSp((p) => { const n = new URLSearchParams(p); n.set('mode', m); return n; }, { replace: true });
  const [rows, setRows] = React.useState<Row[]>(() => draft?.rows ?? [newRow()]);
  const [results, setResults] = React.useState<ProvisionCheckItem[] | null>(() => draft?.results ?? null);
  const [localErrs, setLocalErrs] = React.useState<string[][] | null>(() => draft?.localErrs ?? null);
  // 检查时参与的行（id，顺序同 results / localErrs）：结果按行 id 对回表格，删行、插行后不错位到别的行
  const [checked, setChecked] = React.useState<string[]>(() => draft?.checked ?? []);
  const [sig, setSig] = React.useState(() => draft?.sig ?? '');
  const [confirm, setConfirm] = React.useState(false);
  const [created, setCreated] = React.useState<{ machine_id: string; job_id: string; name: string }[] | null>(null);
  const check = useProvisionCheck(); const provision = useProvision();
  const hostParam = sp.get('host');
  // 宿主机候选取自整份机器列表，订阅着时列表随实时推送最多每 2 秒整表重取（批量上架期间一直在推）：只在用得到时订阅——
  // 单台开了「在已有机器上添加 IP」、批量（哪一行都能填宿主机）；带着 ?host= 等预填时总要取（停在「已开始上架」面板时从抽屉点「添加 IP 节点…」也要能预填）
  const hosts = useHostChoices(hostParam != null || (!created && (mode === 'batch' || rows[0].add_ip))).data ?? NONE_HOSTS;
  const ctx: Ctx = React.useMemo(() => ({ teams, relays, hosts, providers, plans }), [teams, relays, hosts, providers, plans]);

  // ?host=<宿主机 id>：宿主机候选到了之后预填——单台、VPS 节点、打开「在已有机器上添加 IP」，团队与线路机带出宿主机的；预填完把参数从地址里删掉
  // （替换当前历史）。抽屉开在上架页时点「添加 IP 节点」只换地址参数、页面不重挂，跟着参数走，同一台宿主机再点一次也会再预填
  React.useEffect(() => {
    const h = hosts.find((x) => x.id === hostParam);
    if (!h) return;
    const pre = newRow({ kind: 'vps', add_ip: true, host_id: h.id, team_id: h.team_id, relay_id: h.relay_id });
    setResults(null); setLocalErrs(null); setCreated(null);
    // 表里已有填好的（离开过又回来的草稿）：不覆盖，切到批量、把这台接在后面（顶掉末尾的空行）
    if (rows.some((r) => !isBlank(r))) setRows((rs) => (isBlank(rs[rs.length - 1]) ? [...rs.slice(0, -1), pre] : [...rs, pre]));
    else setRows([pre]);
    setSp((p) => { const n = new URLSearchParams(p); n.delete('host'); n.set('mode', rows.some((r) => !isBlank(r)) ? 'batch' : 'single'); return n; }, { replace: true });
  }, [hostParam, hosts, setSp]); // eslint-disable-line react-hooks/exhaustive-deps -- 只在宿主机参数到达时预填一次，rows 读当时的值

  // 草稿随改随存进模块；上架成功（created）后清掉，再进来是空表
  React.useEffect(() => { draft = created ? null : { mode, rows, results, localErrs, checked, sig }; }, [created, mode, rows, results, localErrs, checked, sig]);

  // 批量里没动过的空行（切到批量补的、「加 5 行」多出来的）不参与检查和上架；全是空行时留第一行，让它报「请填写 IP」
  const active = React.useMemo(() => {
    if (mode === 'single') return rows.slice(0, 1);
    const filled = rows.filter((r) => !isBlank(r));
    return filled.length ? filled : rows.slice(0, 1);
  }, [mode, rows]);
  const items = React.useMemo(() => active.map((r) => toItem(r)), [active]);
  const curSig = JSON.stringify([items, active.map((r) => r.unresolved)]);
  const stale = results != null && sig !== curSig;
  const passed = results != null && !stale && results.every((r) => r.ok);
  const resOf = (id: string) => { const i = checked.indexOf(id); return i < 0 ? undefined : results?.[i]; };
  const errOf = (id: string) => { const i = checked.indexOf(id); return i < 0 ? undefined : localErrs?.[i]; };
  const setRow = (i: number, p: Partial<Row>) => setRows((rs) => rs.map((r, j) => (j === i ? { ...r, ...p } : r)));

  // 检查出错后滚到第一个错误并聚焦（错误常在折叠线以下，点了「检查」页面却像没反应）：渲染出错误之后再找
  const page = React.useRef<HTMLDivElement>(null);
  const [jump, setJump] = React.useState(0);
  React.useEffect(() => {
    if (!jump) return;
    const el = page.current?.querySelector<HTMLElement>('.field:has(.hint.is-error), tr.is-row-bad');
    if (!el) return;
    el.scrollIntoView({ block: 'center', behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' });
    el.querySelector<HTMLElement>('td.is-bad .gcell, input:not(:disabled), button:not(:disabled), .gcell:not(:disabled)')?.focus({ preventScroll: true });
  }, [jump]);
  // 内容一改，上一次检查的通知就作废
  React.useEffect(() => { toast.dismiss(CHECK_TOAST); }, [curSig]);

  const runCheck = async () => {
    const errs = active.map(localErrors);
    setLocalErrs(errs); setChecked(active.map((r) => r.id));
    const bad = errs.filter((e) => e.length).length;
    if (bad) { setResults(null); setJump((n) => n + 1); checkToast('bad', mode === 'single' ? '信息不完整，见标红的字段' : `${bad} 行信息不完整，见「检查结果」列`); return; }
    try {
      const r = await check.mutateAsync(items);
      setResults(r.items); setSig(curSig);
      const failed = r.items.filter((x) => !x.ok).length;
      const warns = warnCount(r.items);
      if (!failed) checkToast('ok', `检查通过：${r.items.length} 台${warns ? `，${warns} 条提醒见「检查结果」` : ''}`);
      else { setJump((n) => n + 1); checkToast('bad', mode === 'single' ? '检查未通过，见标红的字段' : `${failed} 行未通过，见「检查结果」列`); }
    } catch (e) { checkToast('bad', errMessage(e)); }
  };
  const start = async () => {
    const r = await provision.mutateAsync(items);
    draft = null; // 确认框转圈时就离开了页面（组件已卸载、effect 不再跑）也要清掉，回来不能再提交一遍
    setCreated(r.jobs);
  };
  const reset = () => { setRows([newRow()]); setResults(null); setLocalErrs(null); setCreated(null); };
  const switchMode = (m: 'single' | 'batch') => {
    setMode(m); setResults(null); setLocalErrs(null); toast.dismiss(CHECK_TOAST);
    // 批量里是不是同机 IP 节点只看宿主机格：单台开了开关却没选宿主机的，切过去不再按同机 IP 节点置灰（宿主机格是「—」）
    if (m === 'batch') setRows((rs) => {
      const xs = rs.map((r) => (r.add_ip && !(r.host_id || r.unresolved.host_id) ? { ...r, add_ip: false } : r));
      return xs.length >= 3 ? xs : [...xs, ...Array.from({ length: 3 - xs.length }, () => newRow({ kind: xs[0]?.kind, team_id: xs[0]?.team_id, relay_id: xs[0]?.relay_id }))];
    });
  };

  if (created) {
    return (
      <div className="stack">
        <PageHead title="上架" />
        <Panel title="已开始上架" sub={`${created.length} 个作业`} flush>
          <ul style={{ listStyle: 'none', margin: 0, padding: 0 }}>
            {created.map((j) => (
              <li key={j.job_id} className="row row-between" style={{ padding: '10px 16px', borderTop: '1px solid var(--line)' }}>
                <span className="row"><CheckCircle2 size={16} style={{ color: 'var(--ok)' }} /><b>{j.name}</b></span>
                <Link to={`/jobs/${j.job_id}`} className="btn btn-secondary btn-sm">查看作业</Link>
              </li>
            ))}
          </ul>
          <div className="panel-foot"><Button onClick={reset}>继续上架</Button><Link to="/jobs" className="btn btn-primary btn-md">去作业页</Link></div>
        </Panel>
      </div>
    );
  }

  const single = mode === 'single';
  // 单台不说「行」：「信息不完整」「未通过」
  const summary = results == null ? (localErrs?.some((e) => e.length) ? <Pill tone="bad">{single ? '信息不完整' : '有行信息不完整'}</Pill> : null)
    : stale ? <Pill tone="warn">内容已修改，需要重新检查</Pill>
    : passed ? <><Pill tone="ok">检查通过，可以开始</Pill>{warnCount(results) > 0 && <Pill tone="warn">{warnCount(results)} 条提醒</Pill>}</>
    : <Pill tone="bad">{single ? '未通过' : `${results.filter((r) => !r.ok).length} 行未通过`}</Pill>;
  // 确认说明：同机 IP 节点不收登录信息、不装 agent，只在宿主机上装它自己的 SS 实例
  const ipRows = active.filter(isIpNode).length;
  const confirmTip = [
    ipRows < active.length && '会用填写的登录信息连接每台机器并安装 agent。密码只在上架作业中使用一次，成功后丢弃。',
    ipRows > 0 && '同机 IP 节点用中控密钥登录宿主机：先核对 IP 在宿主机网卡上、出站走的是它自己，再装上它自己的 Shadowsocks 实例，agent 用宿主机的。',
  ].filter(Boolean).join('\n');
  const confirmDesc = <>{ipRows === active.length ? '会在宿主机上装 SS 实例' : '会登录每台机器并安装 agent'} <InfoTip text={confirmTip} /></>;
  const hostName = (id: string) => hosts.find((h) => h.id === id)?.name ?? '';

  return (
    <div className="stack" ref={page}>
      <PageHead title="上架"
        tools={<Segmented label="模式" value={mode} onChange={switchMode} items={[{ value: 'single', label: '单台' }, { value: 'batch', label: '批量' }]} />} />
      <div className={mode === 'single' ? 'prov-steps is-form' : 'prov-steps'}>
        <span className="s" data-on={passed ? undefined : '1'} data-done={passed ? '1' : undefined}><span className="n">1</span>检查</span><span>→</span>
        <span className="s" data-on={passed ? '1' : undefined}><span className="n">2</span>开始</span>
        <InfoTip text="检查不会改动任何东西；开始时只需确认一次。" />
      </div>

      {mode === 'single'
        ? <SingleForm key={rows[0].id} row={rows[0]} onChange={(p) => setRow(0, p)} ctx={ctx} result={resOf(rows[0].id)} errors={errOf(rows[0].id)} stale={stale} />
        : <BatchGrid rows={rows} setRows={setRows} ctx={ctx} resOf={resOf} errOf={errOf} checkedAny={results != null} stale={stale} />}

      <StickyBar>
        <div className="row row-between">
          <div className="row">{summary}{!single && <span className="t-sub">共 {active.length} 台</span>}</div>
          <div className="row">
            {/* 单台的清空（批量的在表格面板上）：清空后表单是空的，关页面不再提醒有未提交的内容 */}
            {single && <Button variant="quiet" onClick={reset}>清空</Button>}
            <Button onClick={runCheck} loading={check.isPending} variant={passed ? 'secondary' : 'primary'}><ListChecks />检查</Button>
            <Button variant="primary" disabled={!passed} onClick={() => setConfirm(true)}>开始上架</Button>
          </div>
        </div>
      </StickyBar>

      <ConfirmDialog open={confirm} onOpenChange={setConfirm} size="md" title={`确认上架 ${active.length} 台机器？`} confirmLabel="确认上架"
        description={confirmDesc}
        onConfirm={start}>
        <ul className="reason-list" style={{ maxHeight: 220, overflow: 'auto' }}>
          {results?.map((r, i) => (
            <li key={i}><span className="dot" /><span><b>{r.name}</b> <span className="t-sub">
              {isIpNode(active[i]) ? `同机 · ${hostName(active[i].host_id)}` : kindLabel(active[i].kind)} · {active[i].ip}{r.relay_port ? ` · 入口端口 ${r.relay_port}` : ''}{r.ss_port ? ` · SS 端口暂定 ${r.ss_port}` : ''}
            </span></span></li>
          ))}
        </ul>
      </ConfirmDialog>
    </div>
  );
}

/** 底部吸附条（检查 / 开始上架）：挂着时通知抬高让开它 */
function StickyBar({ children }: { children: React.ReactNode }) {
  const ref = React.useRef<HTMLDivElement>(null);
  useToastLift(ref);
  return <div ref={ref} className="panel sticky-bar" style={{ padding: '10px 16px' }}>{children}</div>;
}

/** 宿主机搜索：名称或 IP 以输入开头的排最前，其次是名称 / IP 里连着出现的，其余不列（lib/match） */
const hostFilter = (_: string, search: string, keywords?: string[]) => prefixScore(keywords ?? [], search.trim().toLowerCase());

/** 宿主机选择：在用的 VPS 节点可能上百台，弹出可搜索的列表（名称、IP 都能搜），每项写名称与 IP */
function HostSelect({ id, value, onChange, hosts }: { id?: string; value: string; onChange: (v: string) => void; hosts: HostChoice[] }) {
  const [open, setOpen] = React.useState(false);
  const cur = hosts.find((h) => h.id === value);
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger asChild>
        <button type="button" id={id} className="select-trigger cs-trigger">
          <span className={cur ? undefined : 't-muted'}>{cur ? <>{cur.name} <span className="t-sub">{cur.ip}</span></> : '选择 VPS 节点'}</span>
          <ChevronDown />
        </button>
      </PopoverTrigger>
      <PopoverContent className="cs-pop hs-pop" align="start">
        {open && (
          <Command loop filter={hostFilter}>
            <CommandInput placeholder="搜索名称或 IP" />
            <CommandList>
              <CommandEmpty>没有匹配的 VPS 节点</CommandEmpty>
              {hosts.map((h) => (
                <CommandItem key={h.id} value={h.id} keywords={[h.name, h.ip]} onSelect={(v) => { setOpen(false); if (v !== value) onChange(v); }}>
                  <span className="cmd-main">{h.name}</span><span className="cmd-aside num">{h.ip}</span>{h.id === value && <Check className="cs-check" />}
                </CommandItem>
              ))}
            </CommandList>
          </Command>
        )}
      </PopoverContent>
    </Popover>
  );
}

/* ------------------------------------------------------------------ 公共：下拉选项 */
type Opts = { value: string; label: string }[];
/** 下拉选项按 ctx 缓存：ctx 不变就是同一个数组引用（批量网格里几百个单元格共用，memo 才生效） */
function useOpts(ctx: Ctx) {
  const base = React.useMemo(() => ({
    team: ctx.teams.map((t) => ({ value: t.id, label: t.name })),
    relay: ctx.relays.map((r) => ({ value: r.id, label: r.name })),
    host: ctx.hosts.map((h) => ({ value: h.id, label: `${h.name} · ${h.ip}` })), // 批量格是原生下拉，名称后带 IP 好认
    provider: ctx.providers.map((p) => ({ value: p.id, label: p.name })),
  }), [ctx.teams, ctx.relays, ctx.hosts, ctx.providers]);
  // 套餐选项依赖所选供应商：按供应商缓存
  const planFor = React.useMemo(() => {
    const cache = new Map<string, Opts>();
    return (providerId: string): Opts => {
      let v = cache.get(providerId);
      if (!v) { v = ctx.plans.filter((p) => !p.archived_at && (!providerId || p.provider_id === providerId)).map((p) => ({ value: p.id, label: p.name })); cache.set(providerId, v); }
      return v;
    };
  }, [ctx.plans]);
  return { ...base, planFor };
}

/* ------------------------------------------------------------------ 单台表单 */
function SingleForm({ row, onChange, ctx, result, errors, stale }: { row: Row; onChange: (p: Partial<Row>) => void; ctx: Ctx; result?: ProvisionCheckItem; errors?: string[]; stale: boolean }) {
  const o = useOpts(ctx);
  const k = row.kind;
  // 同机 IP 节点：SSH 端口、首次登录、SS 端口、供应商、套餐不适用，换成禁用的格子写明怎么定（沿用宿主机 / 中控分配 / 不带套餐）
  const ip = isIpNode(row);
  const host = ip ? ctx.hosts.find((h) => h.id === row.host_id) : undefined;
  // 选了宿主机：团队与线路机带出宿主机的（之后可改）
  const pickHost = (v: string) => { const h = ctx.hosts.find((x) => x.id === v); onChange(h ? { host_id: v, team_id: h.team_id, relay_id: h.relay_id } : { host_id: v }); };
  // 上次检查有本地错误时，把错误按字段标在字段旁（按当前内容重算，改对了就消失）；服务端检查没通过的，按错误文字对回字段（结果过期就不标）。
  // IP、端口失焦即查格式与范围，不等点「检查」
  const [blurred, setBlurred] = React.useState<ReadonlySet<keyof Row>>(() => new Set());
  const blur = (k: keyof Row) => () => setBlurred((s) => (s.has(k) ? s : new Set(s).add(k)));
  const fe: Partial<Record<keyof Row, string>> = {};
  if (errors?.length) for (const x of localIssues(row)) fe[x.key] ??= x.msg;
  if (result && !result.ok && !stale) for (const msg of result.errors) { const k = serverField(msg); if (k) fe[k] ??= msg; }
  if (blurred.has('ip') && row.ip.trim()) fe.ip ??= ipIssue(row) ?? undefined;
  for (const k of ['ssh_port', 'ss_port', 'up_port'] as const) if (blurred.has(k)) fe[k] ??= portIssue(row, k) ?? undefined;
  const T = (key: keyof Row, label: string, p?: { info?: string; ph?: string; num?: boolean; check?: boolean }) => (
    <Field label={label} info={p?.info} error={fe[key]}>
      <Input inputMode={p?.num ? 'numeric' : undefined} placeholder={p?.ph} value={row[key] as string} aria-label={label} aria-invalid={fe[key] ? true : undefined} onBlur={p?.check ? blur(key) : undefined}
        onChange={(e) => onChange({ [key]: p?.num ? e.target.value.replace(/\D/g, '') : e.target.value })} />
    </Field>
  );
  // 同机 IP 节点不适用的格子：禁用、显示取自哪里的值，说明进 (i)
  const Fixed = (label: string, info: string, value: string, ph = '') => <Field label={label} info={info}><Input disabled value={value} placeholder={ph} aria-label={label} /></Field>;
  const plan = ip ? undefined : ctx.plans.find((p) => p.id === row.plan_id);
  const pubkeyBtn = meta().ssh_public_key ? <PubkeyButton /> : null;
  // 「中控密钥登录不了」的原话带着整行公钥，标在字段旁会把表单撑宽：这里只说结论，原话（含公钥）在下面的检查结果里
  const loginErr = fe.login_pw && '中控密钥登录不了，也没填 root 密码：填写 root 密码，或在供应商面板录入中控公钥';
  return (
    <div className="stack prov-form">
      <Panel title="机器">
        <div className="stack">
          <Segmented label="类型" value={k} onChange={(v) => onChange({ kind: v })} items={KIND_OPTS} />
          {k === 'vps' && (
            <div className="row" style={{ gap: 4 }}>
              <label className="check-row" style={{ gap: 10 }}>
                <Switch checked={row.add_ip} onCheckedChange={(v) => onChange(v ? { add_ip: true } : { add_ip: false, host_id: '' })} />
                <span>在已有机器上添加 IP</span>
              </label>
              <InfoTip text="VPS 节点上另配的公网 IP，每个 IP 上架成一台独立节点" />
            </div>
          )}
          <div className="form-grid">
            {ip && <div className="span-2"><Field label="宿主机" info="这个 IP 所在的 VPS 节点；SSH、agent 与供应商都用它的，团队与线路机默认同它（可改）" error={fe.host_id}><HostSelect value={row.host_id} onChange={pickHost} hosts={ctx.hosts} /></Field></div>}
            {T('ip', k === 'proxy' ? '出口 IP' : 'IP', { ph: '198.51.100.20', info: ip ? '要直接配在宿主机网卡上（EIP / NAT 映射的不行），检查时会登录宿主机核对' : undefined, check: true })}
            <Field label="国家" error={fe.country}>
              <CountrySelect value={row.country} onChange={(v) => onChange({ country: v })} autoLabel="自动（按 IP）" aria-label="国家" />
            </Field>
            {k !== 'proxy' && (ip ? Fixed('SSH 端口', '同机 IP 节点经宿主机登录，用宿主机的 SSH 端口', host ? String(host.ssh_port) : '') : T('ssh_port', 'SSH 端口', { ph: '22', num: true, check: true }))}
            {k === 'relay' ? <div className="span-2">{T('label', '线路机名', { ph: '线路机3' })}</div> : (
              <>
                <Field label="团队" error={fe.team_id}><Select value={row.team_id} onChange={(v) => onChange({ team_id: v })} options={o.team} aria-label="团队" /></Field>
                <Field label="编号" error={fe.number}><Input inputMode="numeric" placeholder="自动（接在末尾）" value={row.number} onChange={(e) => onChange({ number: e.target.value.replace(/\D/g, '') })} /></Field>
                <Field label="挂在线路机" error={fe.relay_id}><Select value={row.relay_id} onChange={(v) => onChange({ relay_id: v })} options={o.relay} aria-label="挂在线路机" /></Field>
                {k === 'vps' && (ip ? Fixed('SS 端口', '由中控在宿主机上分配空闲端口', '', `自动分配（${meta().defaults.ss_port + 1} 起）`) : T('ss_port', 'SS 端口', { ph: String(meta().defaults.ss_port), num: true, check: true, info: 'NAT 机填供应商映射后的公网端口' }))}
              </>
            )}
            {/* 同机 IP 节点：中控用自己的密钥登录宿主机，没有要填的 */}
            {k !== 'proxy' && !ip && (
              // 「中控密钥登录不了」标在这里时复制入口也留着（错误替换提示的位置）
              <Field label="root 密码" info="首次登录用，只在上架作业中用一次，成功后丢弃，不保存。不填时检查会用中控密钥试登录" error={loginErr && <>{loginErr} {pubkeyBtn}</>}
                hint={pubkeyBtn && <>{pubkeyBtn} {PUBKEY_TIP}</>}>
                <Input type="password" autoComplete="new-password" placeholder="留空用中控密钥" value={row.login_pw} aria-invalid={fe.login_pw ? true : undefined} onChange={(e) => onChange({ login_pw: e.target.value })} />
              </Field>
            )}
          </div>
        </div>
      </Panel>

      {k === 'proxy' && (
        <Panel title="上游代理">
          <div className="form-grid">
            <Field label="协议"><Select value={row.up_protocol} onChange={(v) => onChange({ up_protocol: v })} options={[{ value: 'socks5', label: 'SOCKS5' }, { value: 'http', label: 'HTTP' }]} /></Field>
            {T('up_host', '主机', { ph: 'proxy.example.net' })}
            {T('up_port', '端口', { num: true, ph: '1080', check: true })}
            {T('up_user', '用户名', { ph: '选填' })}
            <Field label="密码"><Input type="password" autoComplete="new-password" placeholder="选填" value={row.up_pw} onChange={(e) => onChange({ up_pw: e.target.value })} /></Field>
          </div>
        </Panel>
      )}

      <Panel title="来源与订阅" sub={<InfoTip text={ip ? '默认不单独记费用：供应商对附加 IP 另收费时再填到期日与费用' : '选了套餐只需填到期日，其余跟随套餐与供应商'} />}>
        <div className="form-grid">
          {ip ? Fixed('供应商', '沿用宿主机的供应商', host?.provider ?? '', '—') : <Field label="供应商" error={fe.provider_id}><Select value={row.provider_id} onChange={(v) => onChange({ provider_id: v, plan_id: '' })} options={o.provider} allowEmpty="不设置" /></Field>}
          {ip ? Fixed('套餐', '同机 IP 节点不带套餐，附加 IP 另收费时填到期日与费用', '', '不带套餐') : <Field label="套餐" error={fe.plan_id}><Select value={row.plan_id} onChange={(v) => onChange({ plan_id: v })} options={o.planFor(row.provider_id)} allowEmpty="不设置" /></Field>}
          {plan && (() => { const s = inheritSummary(plan, ctx.providers.find((p) => p.id === plan.provider_id)); return <div className="span-2 hint">{s.text} <InfoTip text={s.tip} /></div>; })()}
          {T('service_ref', '服务编号', { ph: 'SVC-1042' })}
          <Field label={<>到期日{plan && <span className="field-tag">必填</span>}</>} info={plan ? undefined : '填了就记费用、到期提醒'} error={fe.due_date}><DateField value={row.due_date} onChange={(v) => onChange({ due_date: v })} /></Field>
          {T('price', '费用', { ph: ip ? '附加 IP 的费用' : plan ? `跟随套餐（${F.money(plan.price_cents, plan.currency)}）` : '没有套餐时要填', info: plan ? '留空跟随套餐；填了 = 这台单独设置' : undefined })}
          <div className="span-2"><Field label="备注"><Input value={row.note} onChange={(e) => onChange({ note: e.target.value })} /></Field></div>
        </div>
      </Panel>
      <CheckOutput rows={[row]} resOf={() => result} errOf={() => errors} stale={stale} />
    </div>
  );
}

/** 检查结果列表：rows 是表格里的行（行号按它算），没参与检查的空行没有结果、不出现 */
function CheckOutput({ rows, resOf, errOf, stale }: { rows: Row[]; resOf: (id: string) => ProvisionCheckItem | undefined; errOf: (id: string) => string[] | undefined; stale: boolean }) {
  const lines: React.ReactNode[] = [];
  rows.forEach((r, i) => {
    const le = errOf(r.id) ?? [];
    const res = resOf(r.id);
    if (le.length) lines.push(<li key={i}><TriangleAlert size={14} style={{ color: 'var(--bad)', marginTop: 3 }} /><span>{rows.length > 1 ? `第 ${i + 1} 行：` : ''}{le.join('；')}</span></li>);
    else if (res && !res.ok) lines.push(<li key={i}><TriangleAlert size={14} style={{ color: 'var(--bad)', marginTop: 3 }} /><span>{rows.length > 1 ? `第 ${i + 1} 行：` : ''}{res.errors.join('；')}</span></li>);
    else if (res) lines.push(<li key={i}><CheckCircle2 size={14} style={{ color: 'var(--ok)', marginTop: 3 }} /><span>{rows.length > 1 ? `第 ${i + 1} 行：` : ''}将创建 <b>{res.name}</b>{res.relay_port ? `，入口端口 ${res.relay_port}` : ''}{res.ss_port ? `，SS 端口暂定 ${res.ss_port}` : ''}</span></li>);
    // 提醒（发行版已过标准支持期等）不影响通过，跟在这一行的结果后面
    if (!le.length) res?.warnings?.forEach((w, j) => lines.push(<li key={`${i}-w${j}`}><Info size={14} style={{ color: 'var(--warn)', marginTop: 3 }} /><span>{rows.length > 1 ? `第 ${i + 1} 行提醒：` : '提醒：'}{w}</span></li>));
  });
  if (!lines.length) return null;
  return (
    <Panel title="检查结果" sub={stale ? '内容已修改，结果已过期' : undefined}>
      {/* 中控公钥是一长串不断行的字符：窄屏上任意处折行，不撑出横向滚动 */}
      <ul className="reason-list" style={{ opacity: stale ? 0.5 : 1, overflowWrap: 'anywhere' }}>{lines}</ul>
    </Panel>
  );
}

/* ------------------------------------------------------------------ 批量网格 */
function BatchGrid({ rows, setRows, ctx, resOf, errOf, checkedAny, stale }: { rows: Row[]; setRows: React.Dispatch<React.SetStateAction<Row[]>>; ctx: Ctx; resOf: (id: string) => ProvisionCheckItem | undefined; errOf: (id: string) => string[] | undefined; checkedAny: boolean; stale: boolean }) {
  const addRows = (n: number) => setRows((rs) => { const last = rs[rs.length - 1]; const base = last ? { kind: last.kind, team_id: last.team_id, relay_id: last.relay_id, provider_id: last.provider_id, plan_id: last.plan_id, add_ip: last.add_ip, host_id: last.host_id } : {}; return [...rs, ...Array.from({ length: n }, () => newRow(base))]; });
  const total = COLS.reduce((s, c) => s + c.w, 0) + 40 + 56 + 220;
  const [askClear, setAskClear] = React.useState(false);

  // 单元格拿到的回调必须稳定（GridCell 是 memo），所以按行 id 工作，不捕获行下标
  const opts = useOpts(ctx);
  const rowsRef = React.useRef(rows); rowsRef.current = rows;
  const change = React.useCallback((rowId: string, p: Partial<Row>) => setRows((rs) => rs.map((x) => (x.id === rowId ? { ...x, ...p } : x))), [setRows]);
  const onPaste = React.useCallback((e: React.ClipboardEvent, rowId: string, ci: number) => {
    const text = e.clipboardData.getData('text');
    if (!/[\t\n]/.test(text.trim())) return; // 单个值走默认粘贴
    e.preventDefault();
    const all = parseTsv(text);
    // 带表头（「复制表头」做的模板）按列名对上，列顺序随意；不带表头从光标所在的列起按表头顺序
    const head = all.length ? headerCols(all[0]) : null;
    const cols = head ?? PASTE_COLS.slice(PASTE_COLS.indexOf(COLS[ci]));
    const grid = head ? all.slice(1) : all;
    const ri = Math.max(0, rowsRef.current.findIndex((r) => r.id === rowId));
    setRows((rs) => {
      const next = [...rs];
      grid.forEach((line, dy) => {
        const idx = ri + dy;
        while (next.length <= idx) next.push(newRow());
        next[idx] = pasteLine(next[idx], line, cols, ctx);
      });
      return next;
    });
  }, [setRows, ctx]);
  const copyHeader = () => void copyText(PASTE_COLS.map((c) => c?.label ?? UNUSED_LABEL).join('\t'), '表头（可粘贴到表格软件当模板）');

  return (
    <div className="stack">
      <Panel title="批量上架" sub={<InfoTip text={BATCH_TIP} />} flush
        tools={<>{meta().ssh_public_key && <Button size="sm" variant="quiet" title={PUBKEY_TIP} onClick={copyPubkey}><KeyRound />复制中控公钥</Button>}<Button size="sm" onClick={copyHeader}><ClipboardPaste />复制表头</Button><Button size="sm" onClick={() => addRows(1)}><Plus />加一行</Button><Button size="sm" onClick={() => addRows(5)}>加 5 行</Button><Button size="sm" variant="quiet" onClick={() => (rows.length > 1 ? setAskClear(true) : setRows([newRow()]))}>清空</Button></>}>
        <div className="table-wrap">
          <table className="dt dt-grid" style={{ minWidth: total }}>
            <colgroup><col style={{ width: 40 }} />{COLS.map((c) => <col key={c.key} style={{ width: c.w }} />)}<col style={{ width: 220 }} /><col style={{ width: 56 }} /></colgroup>
            <thead><tr><th className="c">#</th>{COLS.map((c) => <th key={c.key} style={{ paddingLeft: 8 }}>{c.label}</th>)}<th className="gc-res">检查结果</th><th className="gc-del" /></tr></thead>
            <tbody>
              {rows.map((r, ri) => {
                const le = errOf(r.id) ?? []; const res = resOf(r.id);
                const bad = le.length > 0 || (res && !res.ok);
                const warns = !le.length && res?.warnings?.length ? res.warnings : null;
                return (
                  <tr key={r.id} className={bad ? 'is-row-bad' : undefined}>
                    <td className="idx">{ri + 1}</td>
                    {COLS.map((c, ci) => (
                      <td key={c.key} className={c.key !== 'kind' && !applies(c, r) ? 'is-na' : r.unresolved[c.key] ? 'is-bad' : undefined}>
                        <GridCell col={c} ci={ci} row={r} opts={c.type === 'team' ? opts.team : c.type === 'relay' ? opts.relay : c.type === 'host' ? opts.host : c.type === 'provider' ? opts.provider : c.type === 'plan' ? opts.planFor(r.provider_id) : undefined} onChange={change} onPaste={onPaste} />
                      </td>
                    ))}
                    <td className="gc-res" data-tip={le.length || (res && !res.ok) || warns ? [...le, ...(res?.errors ?? []), ...(warns ?? [])].join('\n') : undefined}>
                      {le.length ? <span style={{ color: 'var(--bad-text)' }}>{le.join('；')}</span>
                        : res ? (res.ok ? <span style={{ color: stale ? 'var(--fg-3)' : 'var(--ok-text)' }}>✓ {res.name}{res.relay_port ? ` · :${res.relay_port}` : ''}{res.ss_port ? ` · SS ${res.ss_port}` : ''}</span> : <span style={{ color: 'var(--bad-text)' }}>{res.errors.join('；')}</span>)
                        : <span className="t-muted">—</span>}
                      {warns && <span style={{ color: stale ? 'var(--fg-3)' : 'var(--warn-text)' }}> · {warns.join('；')}</span>}
                    </td>
                    <td className="c gc-del"><IconButton label={`删除第 ${ri + 1} 行`} size="sm" disabled={rows.length === 1} onClick={() => setRows((rs) => rs.filter((x) => x.id !== r.id))}><Trash2 size={14} /></IconButton></td>
                  </tr>
                );
              })}
            </tbody>
          </table>
        </div>
      </Panel>
      {checkedAny && <CheckOutput rows={rows} resOf={resOf} errOf={errOf} stale={stale} />}
      <ConfirmDialog open={askClear} onOpenChange={setAskClear} title="清空批量表格？" confirmLabel="清空" danger
        description="已填写的内容会丢失，无法撤销" onConfirm={() => setRows([newRow()])} />
    </div>
  );
}

/** 网格单元格：memo。props 里只有 row 会随编辑变化（不可变更新，只有被改的那一行是新对象），其余都是稳定引用，改一格不会重绘整张表 */
const GridCell = React.memo(function GridCell({ col, ci, row, opts, onChange, onPaste }: {
  col: Col; ci: number; row: Row; opts?: Opts; onChange: (rowId: string, p: Partial<Row>) => void; onPaste: (e: React.ClipboardEvent, rowId: string, ci: number) => void;
}) {
  const na = col.key !== 'kind' && !applies(col, row);
  const paste = (e: React.ClipboardEvent) => onPaste(e, row.id, ci);
  // 从下拉里选了：这一格不再是“粘贴进来但没匹配上”的值，去掉它的 unresolved
  const resolved = (key: keyof Row) => { const { [key]: _drop, ...rest } = row.unresolved; return rest; };
  // 换供应商清掉套餐；宿主机格填没填决定这一行是不是同机 IP 节点
  const extra = (key: keyof Row, v: string): Partial<Row> => (key === 'provider_id' ? { plan_id: '' } : key === 'host_id' ? { add_ip: !!v } : {});
  const sel = (value: string, key: keyof Row, empty = '—') => (
    <select className="gcell" value={value} disabled={na} aria-label={col.label} onChange={(e) => onChange(row.id, { [key]: e.target.value, ...extra(key, e.target.value), unresolved: resolved(key) })} onPaste={paste}>
      <option value="">{empty}</option>{(opts ?? []).map((x) => <option key={x.value} value={x.value}>{x.label}</option>)}
    </select>
  );
  switch (col.type) {
    case 'kind': return <select className="gcell" value={row.kind} aria-label={col.label} onChange={(e) => onChange(row.id, { kind: e.target.value as Kind })} onPaste={paste}>{KIND_OPTS.map((x) => <option key={x.value} value={x.value}>{x.label}</option>)}</select>;
    case 'host': return sel(row.host_id, 'host_id');
    case 'team': return sel(row.team_id, 'team_id');
    case 'relay': return sel(row.relay_id, 'relay_id');
    case 'provider': return sel(row.provider_id, 'provider_id');
    case 'plan': return sel(row.plan_id, 'plan_id');
    case 'country': return <CountrySelect className="gcell" value={row.country} autoLabel="自动" aria-label={col.label} onPaste={paste} onChange={(v) => onChange(row.id, { country: v, unresolved: resolved('country') })} />;
    case 'proto': return <select className="gcell" value={row.up_protocol} disabled={na} aria-label={col.label} onChange={(e) => onChange(row.id, { up_protocol: e.target.value })} onPaste={paste}><option value="socks5">SOCKS5</option><option value="http">HTTP</option></select>;
    default: {
      const v = row[col.key as keyof Row] as string;
      const input = <input className="gcell" type={col.type === 'secret' ? 'password' : 'text'} inputMode={col.type === 'num' ? 'numeric' : undefined} autoComplete={col.type === 'secret' ? 'new-password' : 'off'} spellCheck={false}
        value={v} disabled={na} placeholder={na ? '' : col.ph} aria-label={col.label} onPaste={paste} onChange={(e) => onChange(row.id, { [col.key]: col.type === 'num' ? e.target.value.replace(/\D/g, '') : e.target.value } as Partial<Row>)} />;
      // 到期日：文本格照常键入 / 整块粘贴 YYYY-MM-DD，右侧日历按钮弹出选择
      return col.type === 'date' && !na
        ? <div className="gcell-date">{input}<DateField iconOnly value={v} onChange={(d) => onChange(row.id, { due_date: d })} aria-label={`选择${col.label}`} /></div>
        : input;
    }
  }
});
