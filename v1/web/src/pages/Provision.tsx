// 上架：单台表单 + 批量可编辑网格（可从表格软件粘贴）。两步：1 检查（不落库，显示每行错误与分配到的名称/端口）→ 2 开始（唯一一次确认）。
// VPS 节点可以「在已有机器上添加 IP」：同机 IP 节点（host_id），SSH、agent、供应商沿用宿主机的；抽屉里宿主机的「添加 IP 节点」跳来时带 ?host=<id> 预填。
import * as React from 'react';
import { Link, useSearchParams } from 'react-router-dom';
import { CheckCircle2, ClipboardPaste, ListChecks, Plus, Trash2, TriangleAlert } from 'lucide-react';
import { useHostChoices, usePlans, useProviders, useProvision, useProvisionCheck, useRelayChoices, useTeams, type HostChoice } from '@/lib/queries';
import type { Kind, ProvisionCheckItem } from '@/lib/types';
import { KIND_ORDER } from '@/lib/sort';
import { inheritSummary, kindLabel, meta } from '@/lib/derive';
import * as F from '@/lib/format';
import { Button, IconButton } from '@/components/ui/button';
import { Field, InfoTip, Input, Switch } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { DateField } from '@/components/ui/date-field';
import { Segmented } from '@/components/ui/overlays';
import { ConfirmDialog } from '@/components/app/dialogs';
import { toast } from '@/components/ui/toast';
import { copyText } from '@/lib/clipboard';
import { PageHead, Panel, Pill } from '@/components/app/primitives';
import { COLS, PASTE_COLS, UNUSED_LABEL, applies, headerCols, isBlank, isIpNode, localErrors, localIssues, newRow, parseTsv, pasteLine, toItem, type Col, type Ctx, type Row } from '@/features/provision-model';
import { CountrySelect } from '@/components/app/CountrySelect';

// 上架页懒加载、在外壳等到 /meta 之后才求值，类型名、默认端口可以在模块里取
const KIND_OPTS = KIND_ORDER.map((value) => ({ value, label: kindLabel(value) }));

const NONE: never[] = [];
/** 批量表格的粘贴与填写说明（面板标题旁的「(i)」） */
const BATCH_TIP = '可从表格软件整块粘贴（Tab 分隔）：带表头时按列名对上，不带时列顺序同表头。\n编号留空 = 自动编号（接在末尾）。灰色格子对当前类型不适用，粘贴时会自动跳过。\n选择列可直接粘贴名称（如「团队1」「线路机1」），国家粘两位代码或中文名（如「JP」「日本」），留空按 IP 自动。\n填了「宿主机」的 VPS 节点行是同机 IP 节点（宿主机上另配的 IP）：SSH 端口、登录与供应商沿用宿主机的，SS 端口由中控在宿主机上分配，不带套餐，这几格变灰。';
const NONE_RELAYS: { id: string; name: string }[] = [];
const NONE_HOSTS: HostChoice[] = [];

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

  const [mode, setMode] = React.useState<'single' | 'batch'>(() => draft?.mode ?? 'single');
  const [rows, setRows] = React.useState<Row[]>(() => draft?.rows ?? [newRow()]);
  const [results, setResults] = React.useState<ProvisionCheckItem[] | null>(() => draft?.results ?? null);
  const [localErrs, setLocalErrs] = React.useState<string[][] | null>(() => draft?.localErrs ?? null);
  // 检查时参与的行（id，顺序同 results / localErrs）：结果按行 id 对回表格，删行、插行后不错位到别的行
  const [checked, setChecked] = React.useState<string[]>(() => draft?.checked ?? []);
  const [sig, setSig] = React.useState(() => draft?.sig ?? '');
  const [confirm, setConfirm] = React.useState(false);
  const [created, setCreated] = React.useState<{ machine_id: string; job_id: string; name: string }[] | null>(null);
  const check = useProvisionCheck(); const provision = useProvision();
  const [sp, setSp] = useSearchParams();
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
    if (rows.some((r) => !isBlank(r))) {
      setMode('batch');
      setRows((rs) => (isBlank(rs[rs.length - 1]) ? [...rs.slice(0, -1), pre] : [...rs, pre]));
    } else { setMode('single'); setRows([pre]); }
    setSp((p) => { const n = new URLSearchParams(p); n.delete('host'); return n; }, { replace: true });
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

  const runCheck = async () => {
    const errs = active.map(localErrors);
    setLocalErrs(errs); setChecked(active.map((r) => r.id));
    const bad = errs.filter((e) => e.length).length;
    if (bad) { setResults(null); toast.err(mode === 'single' ? '信息不完整，请按字段旁标红的提示修改' : `${bad} 行信息不完整，见表格的「检查结果」列`); return; }
    try {
      const r = await check.mutateAsync(items);
      setResults(r.items); setSig(curSig);
      if (r.items.every((x) => x.ok)) toast.ok(`检查通过：${r.items.length} 台`);
    } catch (e) { toast.err(e); }
  };
  const start = async () => {
    const r = await provision.mutateAsync(items);
    draft = null; // 确认框转圈时就离开了页面（组件已卸载、effect 不再跑）也要清掉，回来不能再提交一遍
    setCreated(r.jobs);
  };
  const reset = () => { setRows([newRow()]); setResults(null); setLocalErrs(null); setCreated(null); };
  const switchMode = (m: 'single' | 'batch') => {
    setMode(m); setResults(null); setLocalErrs(null);
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

  const summary = results == null ? (localErrs?.some((e) => e.length) ? <Pill tone="bad">有行信息不完整</Pill> : <span className="t-sub">填好后先点「检查」</span>)
    : stale ? <Pill tone="warn">内容已修改，需要重新检查</Pill>
    : passed ? <Pill tone="ok">检查通过，可以开始</Pill> : <Pill tone="bad">{results.filter((r) => !r.ok).length} 行未通过</Pill>;
  // 确认说明：同机 IP 节点不收登录信息、不装 agent，只在宿主机上装它自己的 SS 实例
  const ipRows = active.filter(isIpNode).length;
  const confirmTip = [
    ipRows < active.length && '会用填写的登录信息连接每台机器并安装 agent。密码只在上架作业中使用一次，成功后丢弃。',
    ipRows > 0 && '同机 IP 节点用中控密钥登录宿主机：先核对 IP 在宿主机网卡上、出站走的是它自己，再装上它自己的 Shadowsocks 实例，agent 用宿主机的。',
  ].filter(Boolean).join('\n');
  const confirmDesc = <>{ipRows === active.length ? '会在宿主机上装 SS 实例' : '会登录每台机器并安装 agent'} <InfoTip text={confirmTip} /></>;
  const hostName = (id: string) => hosts.find((h) => h.id === id)?.name ?? '';

  return (
    <div className="stack">
      <PageHead title="上架"
        tools={<Segmented label="模式" value={mode} onChange={switchMode} items={[{ value: 'single', label: '单台' }, { value: 'batch', label: '批量' }]} />} />
      <div className="prov-steps">
        <span className="s" data-on={passed ? undefined : '1'} data-done={passed ? '1' : undefined}><span className="n">1</span>检查</span><span>→</span>
        <span className="s" data-on={passed ? '1' : undefined}><span className="n">2</span>开始</span>
        <InfoTip text="检查不会改动任何东西；开始时只需确认一次。" />
      </div>

      {mode === 'single'
        ? <SingleForm row={rows[0]} onChange={(p) => setRow(0, p)} ctx={ctx} result={resOf(rows[0].id)} errors={errOf(rows[0].id)} stale={stale} />
        : <BatchGrid rows={rows} setRows={setRows} ctx={ctx} resOf={resOf} errOf={errOf} checkedAny={results != null} stale={stale} />}

      <div className="panel sticky-bar" style={{ padding: '10px 16px' }}>
        <div className="row row-between">
          <div className="row">{summary}<span className="t-sub">共 {active.length} 台</span></div>
          <div className="row">
            <Button onClick={runCheck} loading={check.isPending} variant={passed ? 'secondary' : 'primary'}><ListChecks />检查</Button>
            <Button variant="primary" disabled={!passed} onClick={() => setConfirm(true)}>开始上架</Button>
          </div>
        </div>
      </div>

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

/* ------------------------------------------------------------------ 公共：下拉选项 */
type Opts = { value: string; label: string }[];
/** 下拉选项按 ctx 缓存：ctx 不变就是同一个数组引用（批量网格里几百个单元格共用，memo 才生效） */
function useOpts(ctx: Ctx) {
  const base = React.useMemo(() => ({
    team: ctx.teams.map((t) => ({ value: t.id, label: t.name })),
    relay: ctx.relays.map((r) => ({ value: r.id, label: r.name })),
    host: ctx.hosts.map((h) => ({ value: h.id, label: h.name })),
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
  // 上次检查有本地错误时，把错误按字段标在字段旁（按当前内容重算，改对了就消失）
  const fe: Partial<Record<keyof Row, string>> = {};
  if (errors?.length) for (const x of localIssues(row)) fe[x.key] ??= x.msg;
  const T = (key: keyof Row, label: string, p?: { hint?: string; info?: string; ph?: string; num?: boolean }) => (
    <Field label={label} hint={p?.hint} info={p?.info} error={fe[key]}>
      <Input inputMode={p?.num ? 'numeric' : undefined} placeholder={p?.ph} value={row[key] as string} onChange={(e) => onChange({ [key]: p?.num ? e.target.value.replace(/\D/g, '') : e.target.value })} />
    </Field>
  );
  const Fixed = (label: string, hint: string, value: string, ph = '') => <Field label={label} hint={hint}><Input disabled value={value} placeholder={ph} /></Field>;
  const plan = ip ? undefined : ctx.plans.find((p) => p.id === row.plan_id);
  return (
    <div className="stack">
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
            {ip && <div className="span-2"><Field label="宿主机" info="这个 IP 所在的 VPS 节点；SSH、agent 与供应商都用它的" error={fe.host_id}><Select value={row.host_id} onChange={pickHost} options={o.host} placeholder="选择在用的落地机" /></Field></div>}
            {T('ip', k === 'proxy' ? '出口 IP' : 'IP', { ph: '198.51.100.20', hint: ip ? '须配在宿主机网卡上' : undefined, info: ip ? 'EIP / NAT 映射的不行' : undefined })}
            <Field label="国家" error={fe.country}>
              <CountrySelect value={row.country} onChange={(v) => onChange({ country: v })} autoLabel="自动（按 IP）" aria-label="国家" />
            </Field>
            {k !== 'proxy' && (ip ? Fixed('SSH 端口', '沿用宿主机', host ? String(host.ssh_port) : '') : T('ssh_port', 'SSH 端口', { ph: '22', num: true }))}
            {k === 'relay' ? <div className="span-2">{T('label', '线路机名', { ph: '线路机3' })}</div> : (
              <>
                <Field label="团队" hint={ip ? '默认同宿主机，可改' : undefined} error={fe.team_id}><Select value={row.team_id} onChange={(v) => onChange({ team_id: v })} options={o.team} /></Field>
                <Field label="编号" error={fe.number}><Input inputMode="numeric" placeholder="自动（接在末尾）" value={row.number} onChange={(e) => onChange({ number: e.target.value.replace(/\D/g, '') })} /></Field>
                <Field label="挂在线路机" hint={ip ? '默认同宿主机，可改' : undefined} error={fe.relay_id}><Select value={row.relay_id} onChange={(v) => onChange({ relay_id: v })} options={o.relay} /></Field>
                {k === 'vps' && (ip ? Fixed('SS 端口', '由中控分配', '', `自动分配（${meta().defaults.ss_port + 1} 起）`) : T('ss_port', 'SS 端口', { ph: String(meta().defaults.ss_port), num: true, info: 'NAT 机填供应商映射后的公网端口' }))}
              </>
            )}
          </div>
        </div>
      </Panel>

      {k === 'proxy' ? (
        <Panel title="上游代理">
          <div className="form-grid">
            <Field label="协议"><Select value={row.up_protocol} onChange={(v) => onChange({ up_protocol: v })} options={[{ value: 'socks5', label: 'SOCKS5' }, { value: 'http', label: 'HTTP' }]} /></Field>
            {T('up_host', '主机', { ph: 'proxy.example.net' })}
            {T('up_port', '端口', { num: true })}
            {T('up_user', '用户名')}
            <Field label="密码"><Input type="password" autoComplete="new-password" value={row.up_pw} onChange={(e) => onChange({ up_pw: e.target.value })} /></Field>
          </div>
        </Panel>
      ) : ip ? (
        <Panel title="首次登录"><div className="hint">沿用宿主机，不需要填 <InfoTip text="中控用自己的密钥登录宿主机。" /></div></Panel>
      ) : (
        <Panel title="首次登录" sub={<InfoTip text="密码只在上架作业中用一次，成功后丢弃，不保存" />}>
          <div className="form-grid">
            <Field label="root 密码"><Input type="password" autoComplete="new-password" placeholder="留空用中控密钥" value={row.login_pw} onChange={(e) => onChange({ login_pw: e.target.value })} /></Field>
          </div>
        </Panel>
      )}

      <Panel title="来源与订阅" sub={<InfoTip text={ip ? '默认不单独记费用：供应商对附加 IP 另收费时再填到期日与费用' : '选了套餐只需填到期日，其余跟随套餐与供应商'} />}>
        <div className="form-grid">
          {ip ? Fixed('供应商', '沿用宿主机', host?.provider ?? '', '—') : <Field label="供应商"><Select value={row.provider_id} onChange={(v) => onChange({ provider_id: v, plan_id: '' })} options={o.provider} allowEmpty="不设置" /></Field>}
          {ip ? Fixed('套餐', '同机 IP 节点不带套餐', '', '不带套餐') : <Field label="套餐"><Select value={row.plan_id} onChange={(v) => onChange({ plan_id: v })} options={o.planFor(row.provider_id)} allowEmpty="不设置" /></Field>}
          {plan && <div className="span-2 hint">{inheritSummary(plan, ctx.providers.find((p) => p.id === plan.provider_id))}</div>}
          {T('service_ref', '服务编号', { ph: 'SVC-1042' })}
          <Field label="到期日" hint={plan ? '必填' : '选填'} info={plan ? undefined : '填了就记费用、到期提醒'} error={fe.due_date}><DateField value={row.due_date} onChange={(v) => onChange({ due_date: v })} /></Field>
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
  });
  if (!lines.length) return null;
  return (
    <Panel title="检查结果" sub={stale ? '内容已修改，结果已过期' : undefined}>
      <ul className="reason-list" style={{ opacity: stale ? 0.5 : 1 }}>{lines}</ul>
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
        tools={<><Button size="sm" onClick={copyHeader}><ClipboardPaste />复制表头</Button><Button size="sm" onClick={() => addRows(1)}><Plus />加一行</Button><Button size="sm" onClick={() => addRows(5)}>加 5 行</Button><Button size="sm" variant="quiet" onClick={() => (rows.length > 1 ? setAskClear(true) : setRows([newRow()]))}>清空</Button></>}>
        <div className="table-wrap">
          <table className="dt dt-grid" style={{ minWidth: total }}>
            <colgroup><col style={{ width: 40 }} />{COLS.map((c) => <col key={c.key} style={{ width: c.w }} />)}<col style={{ width: 220 }} /><col style={{ width: 56 }} /></colgroup>
            <thead><tr><th className="c">#</th>{COLS.map((c) => <th key={c.key} style={{ paddingLeft: 8 }}>{c.label}</th>)}<th style={{ paddingLeft: 8 }}>检查结果</th><th /></tr></thead>
            <tbody>
              {rows.map((r, ri) => {
                const le = errOf(r.id) ?? []; const res = resOf(r.id);
                const bad = le.length > 0 || (res && !res.ok);
                return (
                  <tr key={r.id} className={bad ? 'is-row-bad' : undefined}>
                    <td className="idx">{ri + 1}</td>
                    {COLS.map((c, ci) => (
                      <td key={c.key} className={c.key !== 'kind' && !applies(c, r) ? 'is-na' : r.unresolved[c.key] ? 'is-bad' : undefined}>
                        <GridCell col={c} ci={ci} row={r} opts={c.type === 'team' ? opts.team : c.type === 'relay' ? opts.relay : c.type === 'host' ? opts.host : c.type === 'provider' ? opts.provider : c.type === 'plan' ? opts.planFor(r.provider_id) : undefined} onChange={change} onPaste={onPaste} />
                      </td>
                    ))}
                    <td style={{ padding: '0 8px' }} data-tip={le.concat(res?.errors ?? []).join('；')}>
                      {le.length ? <span style={{ color: 'var(--bad-text)' }}>{le.join('；')}</span>
                        : res ? (res.ok ? <span style={{ color: stale ? 'var(--fg-3)' : 'var(--ok-text)' }}>✓ {res.name}{res.relay_port ? ` · :${res.relay_port}` : ''}{res.ss_port ? ` · SS ${res.ss_port}` : ''}</span> : <span style={{ color: 'var(--bad-text)' }}>{res.errors.join('；')}</span>)
                        : <span className="t-muted">—</span>}
                    </td>
                    <td className="c"><IconButton label={`删除第 ${ri + 1} 行`} size="sm" disabled={rows.length === 1} onClick={() => setRows((rs) => rs.filter((x) => x.id !== r.id))}><Trash2 size={14} /></IconButton></td>
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
