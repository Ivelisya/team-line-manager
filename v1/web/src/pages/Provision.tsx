// 上架：单台表单 + 批量可编辑网格（可从表格软件粘贴）。两步：1 检查（不落库，显示每行错误与分配到的名称/端口）→ 2 开始（唯一一次确认）。
import * as React from 'react';
import { Link } from 'react-router-dom';
import { CheckCircle2, ClipboardPaste, ListChecks, Plus, Trash2, TriangleAlert } from 'lucide-react';
import { usePlans, useProviders, useProvision, useProvisionCheck, useRelayChoices, useTeams } from '@/lib/queries';
import type { Kind, ProvisionCheckItem } from '@/lib/types';
import { KIND_LABEL } from '@/lib/sort';
import { Button, IconButton } from '@/components/ui/button';
import { Field, Input } from '@/components/ui/form';
import { Select } from '@/components/ui/select';
import { DateField } from '@/components/ui/date-field';
import { Segmented } from '@/components/ui/overlays';
import { ConfirmDialog } from '@/components/app/dialogs';
import { toast } from '@/components/ui/toast';
import { copyText } from '@/lib/clipboard';
import { Note, PageHead, Panel, Pill } from '@/components/app/primitives';
import { COLS, applies, localErrors, localIssues, newRow, parseTsv, setCell, toItem, type Col, type Ctx, type Row } from '@/features/provision-model';

const KIND_OPTS = (Object.entries(KIND_LABEL) as [Kind, string][]).map(([value, label]) => ({ value, label }));

const NONE: never[] = [];
const NONE_RELAYS: { id: string; name: string }[] = [];

export function ProvisionPage() {
  const teams = useTeams().data ?? NONE;
  const providers = useProviders().data ?? NONE;
  const plans = usePlans().data ?? NONE;
  // 只能挂在已上线的线路机上。只取 id/名称，引用在内容不变时保持稳定，ctx 不随每次轮询变化（GridCell 的 memo 才有效）
  const relays = useRelayChoices().data ?? NONE_RELAYS;
  const ctx: Ctx = React.useMemo(() => ({ teams, relays, providers, plans }), [teams, relays, providers, plans]);

  const [mode, setMode] = React.useState<'single' | 'batch'>('single');
  const [rows, setRows] = React.useState<Row[]>([newRow()]);
  const [results, setResults] = React.useState<ProvisionCheckItem[] | null>(null);
  const [localErrs, setLocalErrs] = React.useState<string[][] | null>(null);
  const [sig, setSig] = React.useState('');
  const [confirm, setConfirm] = React.useState(false);
  const [created, setCreated] = React.useState<{ machine_id: string; job_id: string; name: string }[] | null>(null);
  const check = useProvisionCheck(); const provision = useProvision();

  const items = React.useMemo(() => rows.map((r) => toItem(r)), [rows]);
  const curSig = JSON.stringify([items, rows.map((r) => r.unresolved)]);
  const stale = results != null && sig !== curSig;
  const passed = results != null && !stale && results.every((r) => r.ok);

  const active = mode === 'single' ? rows.slice(0, 1) : rows;
  const setRow = (i: number, p: Partial<Row>) => setRows((rs) => rs.map((r, j) => (j === i ? { ...r, ...p } : r)));

  const runCheck = async () => {
    const errs = active.map(localErrors);
    setLocalErrs(errs);
    const bad = errs.filter((e) => e.length).length;
    if (bad) { setResults(null); toast.err(mode === 'single' ? '信息不完整，请按字段旁标红的提示修改' : `${bad} 行信息不完整，见表格的「检查结果」列`); return; }
    try {
      const r = await check.mutateAsync(items.slice(0, active.length));
      setResults(r.items); setSig(curSig);
      if (r.items.every((x) => x.ok)) toast.ok(`检查通过：${r.items.length} 台`);
    } catch (e) { toast.err(e); }
  };
  const start = async () => {
    const r = await provision.mutateAsync(items.slice(0, active.length));
    setCreated(r.jobs);
  };
  const reset = () => { setRows([newRow()]); setResults(null); setLocalErrs(null); setCreated(null); };
  const switchMode = (m: 'single' | 'batch') => {
    setMode(m); setResults(null); setLocalErrs(null);
    if (m === 'batch') setRows((rs) => (rs.length >= 3 ? rs : [...rs, ...Array.from({ length: 3 - rs.length }, () => newRow({ kind: rs[0]?.kind, team_id: rs[0]?.team_id, relay_id: rs[0]?.relay_id }))]));
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

  const summary = results == null ? (localErrs?.some((e) => e.length) ? <Pill tone="bad">有行信息不完整</Pill> : <span className="t-sub">填写完成后先点“检查”</span>)
    : stale ? <Pill tone="warn">内容已修改，需要重新检查</Pill>
    : passed ? <Pill tone="ok">检查通过，可以开始</Pill> : <Pill tone="bad">{results.filter((r) => !r.ok).length} 行未通过</Pill>;

  return (
    <div className="stack">
      <PageHead title="上架" note="添加线路机、落地机或家宽代理"
        tools={<Segmented label="模式" value={mode} onChange={switchMode} items={[{ value: 'single', label: '单台' }, { value: 'batch', label: '批量' }]} />} />
      <div className="prov-steps">
        <span className="s" data-on={passed ? undefined : '1'} data-done={passed ? '1' : undefined}><span className="n">1</span>检查</span><span>→</span>
        <span className="s" data-on={passed ? '1' : undefined}><span className="n">2</span>开始</span>
        <span className="t-sub" style={{ marginLeft: 8 }}>检查不会改动任何东西；开始时只需确认一次。</span>
      </div>

      {mode === 'single'
        ? <SingleForm row={rows[0]} onChange={(p) => setRow(0, p)} ctx={ctx} result={results?.[0]} errors={localErrs?.[0]} stale={stale} />
        : <BatchGrid rows={rows} setRows={setRows} ctx={ctx} results={results} errors={localErrs} stale={stale} />}

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
        description="会用填写的登录信息连接每台机器并安装 agent。密码只在上架作业中使用一次，成功后丢弃。"
        onConfirm={start}>
        <ul className="reason-list" style={{ maxHeight: 220, overflow: 'auto' }}>
          {results?.map((r, i) => <li key={i}><span className="dot" /><span><b>{r.name}</b> <span className="t-sub">{KIND_LABEL[active[i].kind]} · {active[i].ip}{r.relay_port ? ` · 入口端口 ${r.relay_port}` : ''}</span></span></li>)}
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
    provider: ctx.providers.map((p) => ({ value: p.id, label: p.name })),
  }), [ctx.teams, ctx.relays, ctx.providers]);
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
  // 上次检查有本地错误时，把错误按字段标在字段旁（按当前内容重算，改对了就消失）
  const fe: Partial<Record<keyof Row, string>> = {};
  if (errors?.length) for (const x of localIssues(row)) fe[x.key] ??= x.msg;
  const T = (key: keyof Row, label: string, p?: { hint?: string; ph?: string; num?: boolean }) => (
    <Field label={label} hint={p?.hint} error={fe[key]}>
      <Input inputMode={p?.num ? 'numeric' : undefined} placeholder={p?.ph} value={row[key] as string} onChange={(e) => onChange({ [key]: p?.num ? e.target.value.replace(/\D/g, '') : e.target.value })} />
    </Field>
  );
  return (
    <div className="stack">
      <Panel title="机器">
        <div className="stack">
          <Segmented label="类型" value={k} onChange={(v) => onChange({ kind: v })} items={KIND_OPTS} />
          <div className="form-grid">
            {T('ip', k === 'proxy' ? '出口 IP' : 'IP', { ph: '198.51.100.20' })}
            {k !== 'proxy' && T('ssh_port', 'SSH 端口', { ph: '22', num: true })}
            {k === 'relay' ? <div className="span-2">{T('label', '线路机名', { ph: '线路机3', hint: '显示名与线路机备注都用它' })}</div> : (
              <>
                <Field label="团队" error={fe.team_id}><Select value={row.team_id} onChange={(v) => onChange({ team_id: v })} options={o.team} /></Field>
                <Field label="编号" hint="留空自动编号（接在末尾）" error={fe.number}><Input inputMode="numeric" placeholder="自动" value={row.number} onChange={(e) => onChange({ number: e.target.value.replace(/\D/g, '') })} /></Field>
                <Field label="挂在线路机" error={fe.relay_id}><Select value={row.relay_id} onChange={(v) => onChange({ relay_id: v })} options={o.relay} /></Field>
                {k === 'vps' && T('ss_port', 'SS 端口', { ph: '8388', num: true, hint: 'NAT 机填供应商映射后的公网端口' })}
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
      ) : (
        <Panel title="首次登录" sub="只在上架作业里用一次，成功后丢弃">
          <div className="form-grid">
            {T('login_user', '用户', { ph: 'root' })}
            <Field label="密码" hint="不填则使用中控的密钥"><Input type="password" autoComplete="new-password" value={row.login_pw} onChange={(e) => onChange({ login_pw: e.target.value })} /></Field>
          </div>
        </Panel>
      )}

      <Panel title="来源与订阅（选填）">
        <div className="form-grid">
          <Field label="供应商"><Select value={row.provider_id} onChange={(v) => onChange({ provider_id: v, plan_id: '' })} options={o.provider} allowEmpty="不设置" /></Field>
          <Field label="套餐"><Select value={row.plan_id} onChange={(v) => onChange({ plan_id: v })} options={o.planFor(row.provider_id)} allowEmpty="不设置" /></Field>
          {T('service_ref', '服务编号', { ph: 'SVC-1042' })}
          <Field label="到期日" error={fe.due_date}><DateField value={row.due_date} onChange={(v) => onChange({ due_date: v })} /></Field>
          {T('price', '费用', { ph: '默认取套餐价格' })}
          <div className="span-2"><Field label="备注"><Input value={row.note} onChange={(e) => onChange({ note: e.target.value })} /></Field></div>
        </div>
      </Panel>
      <CheckOutput results={result ? [result] : null} rows={[row]} errors={errors ? [errors] : null} stale={stale} />
    </div>
  );
}

function CheckOutput({ results, rows, errors, stale }: { results: ProvisionCheckItem[] | null; rows: Row[]; errors: string[][] | null; stale: boolean }) {
  const lines: React.ReactNode[] = [];
  rows.forEach((_r, i) => {
    const le = errors?.[i] ?? [];
    const res = results?.[i];
    if (le.length) lines.push(<li key={i}><TriangleAlert size={14} style={{ color: 'var(--bad)', marginTop: 3 }} /><span>{rows.length > 1 ? `第 ${i + 1} 行：` : ''}{le.join('；')}</span></li>);
    else if (res && !res.ok) lines.push(<li key={i}><TriangleAlert size={14} style={{ color: 'var(--bad)', marginTop: 3 }} /><span>{rows.length > 1 ? `第 ${i + 1} 行：` : ''}{res.errors.join('；')}</span></li>);
    else if (res) lines.push(<li key={i}><CheckCircle2 size={14} style={{ color: 'var(--ok)', marginTop: 3 }} /><span>{rows.length > 1 ? `第 ${i + 1} 行：` : ''}将创建 <b>{res.name}</b>{res.relay_port ? `，入口端口 ${res.relay_port}` : ''}</span></li>);
  });
  if (!lines.length) return null;
  return (
    <Panel title="检查结果" sub={stale ? '内容已修改，结果已过期' : undefined}>
      <ul className="reason-list" style={{ opacity: stale ? 0.5 : 1 }}>{lines}</ul>
    </Panel>
  );
}

/* ------------------------------------------------------------------ 批量网格 */
function BatchGrid({ rows, setRows, ctx, results, errors, stale }: { rows: Row[]; setRows: React.Dispatch<React.SetStateAction<Row[]>>; ctx: Ctx; results: ProvisionCheckItem[] | null; errors: string[][] | null; stale: boolean }) {
  const addRows = (n: number) => setRows((rs) => { const last = rs[rs.length - 1]; const base = last ? { kind: last.kind, team_id: last.team_id, relay_id: last.relay_id, provider_id: last.provider_id, plan_id: last.plan_id, login_user: last.login_user } : {}; return [...rs, ...Array.from({ length: n }, () => newRow(base))]; });
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
    const grid = parseTsv(text);
    const ri = Math.max(0, rowsRef.current.findIndex((r) => r.id === rowId));
    setRows((rs) => {
      const next = [...rs];
      grid.forEach((line, dy) => {
        const idx = ri + dy;
        while (next.length <= idx) next.push(newRow());
        let r = next[idx];
        line.forEach((cell, dx) => {
          const col = COLS[ci + dx]; if (!col) return;
          if (col.key !== 'kind' && !applies(col, r.kind)) return;
          r = setCell(r, col, cell, ctx);
        });
        next[idx] = r;
      });
      return next;
    });
  }, [setRows, ctx]);
  const copyHeader = () => void copyText(COLS.map((c) => c.label).join('\t'), '表头（可粘贴到表格软件当模板）');

  return (
    <div className="stack">
      <Panel title="批量上架" sub="可从表格软件整块粘贴（Tab 分隔，列顺序同表头）" flush
        tools={<><Button size="sm" onClick={copyHeader}><ClipboardPaste />复制表头</Button><Button size="sm" onClick={() => addRows(1)}><Plus />加一行</Button><Button size="sm" onClick={() => addRows(5)}>加 5 行</Button><Button size="sm" variant="quiet" onClick={() => (rows.length > 1 ? setAskClear(true) : setRows([newRow()]))}>清空</Button></>}>
        <div className="table-wrap">
          <table className="dt dt-grid" style={{ minWidth: total }}>
            <colgroup><col style={{ width: 40 }} />{COLS.map((c) => <col key={c.key} style={{ width: c.w }} />)}<col style={{ width: 220 }} /><col style={{ width: 56 }} /></colgroup>
            <thead><tr><th className="c">#</th>{COLS.map((c) => <th key={c.key} style={{ paddingLeft: 8 }}>{c.label}</th>)}<th style={{ paddingLeft: 8 }}>检查结果</th><th /></tr></thead>
            <tbody>
              {rows.map((r, ri) => {
                const le = errors?.[ri] ?? []; const res = results?.[ri];
                const bad = le.length > 0 || (res && !res.ok);
                return (
                  <tr key={r.id} className={bad ? 'is-row-bad' : undefined}>
                    <td className="idx">{ri + 1}</td>
                    {COLS.map((c, ci) => (
                      <td key={c.key} className={c.key !== 'kind' && !applies(c, r.kind) ? 'is-na' : r.unresolved[c.key] ? 'is-bad' : undefined}>
                        <GridCell col={c} ci={ci} row={r} opts={c.type === 'team' ? opts.team : c.type === 'relay' ? opts.relay : c.type === 'provider' ? opts.provider : c.type === 'plan' ? opts.planFor(r.provider_id) : undefined} onChange={change} onPaste={onPaste} />
                      </td>
                    ))}
                    <td style={{ padding: '0 8px' }} data-tip={le.concat(res?.errors ?? []).join('；')}>
                      {le.length ? <span style={{ color: 'var(--bad-text)' }}>{le.join('；')}</span>
                        : res ? (res.ok ? <span style={{ color: stale ? 'var(--fg-3)' : 'var(--ok-text)' }}>✓ {res.name}{res.relay_port ? ` · :${res.relay_port}` : ''}</span> : <span style={{ color: 'var(--bad-text)' }}>{res.errors.join('；')}</span>)
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
      <Note icon={<ClipboardPaste size={14} />}>编号留空 = 自动编号（接在末尾）。灰色格子对当前类型不适用，粘贴时会自动跳过。选择列可直接粘贴名称（如“团队1”“线路机1”）。</Note>
      {results && <CheckOutput results={results} rows={rows} errors={errors} stale={stale} />}
      <ConfirmDialog open={askClear} onOpenChange={setAskClear} title="清空批量表格？" confirmLabel="清空" danger
        description="表格里所有行已填写的内容（含登录密码）都会丢失，无法撤销。" onConfirm={() => setRows([newRow()])} />
    </div>
  );
}

/** 网格单元格：memo。props 里只有 row 会随编辑变化（不可变更新，只有被改的那一行是新对象），其余都是稳定引用，改一格不会重绘整张表 */
const GridCell = React.memo(function GridCell({ col, ci, row, opts, onChange, onPaste }: {
  col: Col; ci: number; row: Row; opts?: Opts; onChange: (rowId: string, p: Partial<Row>) => void; onPaste: (e: React.ClipboardEvent, rowId: string, ci: number) => void;
}) {
  const na = col.key !== 'kind' && !applies(col, row.kind);
  const paste = (e: React.ClipboardEvent) => onPaste(e, row.id, ci);
  // 从下拉里选了：这一格不再是“粘贴进来但没匹配上”的值，去掉它的 unresolved
  const resolved = (key: keyof Row) => { const { [key]: _drop, ...rest } = row.unresolved; return rest; };
  const sel = (value: string, key: keyof Row, empty = '—') => (
    <select className="gcell" value={value} disabled={na} aria-label={col.label} onChange={(e) => onChange(row.id, { [key]: e.target.value, ...(key === 'provider_id' ? { plan_id: '' } : {}), unresolved: resolved(key) })} onPaste={paste}>
      <option value="">{empty}</option>{(opts ?? []).map((x) => <option key={x.value} value={x.value}>{x.label}</option>)}
    </select>
  );
  switch (col.type) {
    case 'kind': return <select className="gcell" value={row.kind} aria-label={col.label} onChange={(e) => onChange(row.id, { kind: e.target.value as Kind })} onPaste={paste}>{KIND_OPTS.map((x) => <option key={x.value} value={x.value}>{x.label}</option>)}</select>;
    case 'team': return sel(row.team_id, 'team_id');
    case 'relay': return sel(row.relay_id, 'relay_id');
    case 'provider': return sel(row.provider_id, 'provider_id');
    case 'plan': return sel(row.plan_id, 'plan_id');
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
