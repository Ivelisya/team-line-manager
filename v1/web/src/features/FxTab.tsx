// 设置「汇率」：计费统一按美元展示。价格照供应商收的币种录入，展示时中控按这里的汇率折成 USD（月费、合计）。
// 默认每天自动从公开汇率源（ECB 参考汇率）更新；某个币种可以手动设置，设置后自动更新不再改它，可以取消改回自动的值。
// 没有汇率的币种不折算（不当 1:1），各处标「未折算」。
import * as React from 'react';
import { Pencil, Plus, RefreshCw, RotateCcw } from 'lucide-react';
import { useClearFxRate, useFxRates, useRefreshFx, useSetFxRate } from '@/lib/fx';
import type { FxRates } from '@/lib/types';
import * as F from '@/lib/format';
import { Button } from '@/components/ui/button';
import { Field, InfoTip, Input } from '@/components/ui/form';
import { Segmented } from '@/components/ui/overlays';
import { SkeletonRows } from '@/components/ui/skeleton';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog, FormDialog } from '@/components/app/dialogs';
import { ErrorNote, Note, Panel, Pill, TableWrap } from '@/components/app/primitives';
import { ShowAll } from '@/components/app/Pagination';
import { resetOnOpen, useHeld } from '@/lib/useHeld';

type Row = FxRates['rates'][number];
/** 汇率的数字：4 位有效数字 */
const n4 = (v: number) => String(Number(v.toPrecision(4)));

function SourcePill({ r, date }: { r: Row; date: string | null }) {
  switch (r.source) {
    case 'base': return <Pill tone="neutral" dot="none">基准</Pill>;
    case 'manual': return <Pill tone="info" dot="none" tip={`${r.manual?.by ?? ''} 于 ${F.dt(r.manual?.at)} 手动设置${r.auto_per_usd != null ? `\n自动汇率是 1 USD = ${n4(r.auto_per_usd)} ${r.currency}` : ''}`}>手动</Pill>;
    case 'auto': return <Pill tone="ok" dot="none" tip={date ? `数据日期 ${date}` : undefined}>自动</Pill>;
    default: return <Pill tone="warn" dot="none" tip="没有这个币种的汇率，费用不会折成美元，各处显示「未折算」">没有汇率</Pill>;
  }
}

export function FxTab() {
  const q = useFxRates();
  const refresh = useRefreshFx(); const clear = useClearFxRate();
  const [edit, setEdit] = React.useState<{ currency: string } | 'new' | null>(null);
  const editHeld = useHeld(edit === 'new' ? 'new' : edit?.currency, edit);
  const [ask, setAsk] = React.useState<string | null>(null);
  const askHeld = useHeld(ask, ask);
  const [all, setAll] = React.useState(false);
  const d = q.data;
  // 在用的（套餐、在用机器的币种）和手动设置过的先列；其余有自动汇率的币种折叠
  const main = d?.rates.filter((r) => r.in_use || r.source === 'manual' || r.source === 'base') ?? [];
  const rest = d?.rates.filter((r) => !(r.in_use || r.source === 'manual' || r.source === 'base')) ?? [];
  const rows = all ? [...main, ...rest] : main;
  return (
    <>
      <Panel flush title={<>汇率<InfoTip text={'价格按供应商的币种录入，月费和合计按这里的汇率折成美元\n汇率每天自动更新（来自欧洲央行）。手动设置的不会被覆盖，随时可以改回自动\n没有汇率的币种不折算，各处显示「未折算」'} /></>}
        tools={<><Button onClick={() => setEdit('new')}><Plus />手动设置其他币种</Button>
          <Button variant="primary" loading={refresh.isPending} onClick={() => refresh.mutate(undefined, { onSuccess: () => toast.ok('汇率已更新'), onError: toast.err })}><RefreshCw />立即更新</Button></>}>
        {q.isError && <div style={{ padding: 'var(--sp-12) var(--card-pad)' }}><ErrorNote error={q.error} onRetry={() => void q.refetch()} /></div>}
        {q.isLoading ? <SkeletonRows rows={3} head /> : d && (
          <>
            <div className="stack" style={{ padding: 'var(--sp-12) var(--card-pad)', gap: 'var(--sp-8)' }}>
              <div className="t-sub">
                {d.fetched_at ? <>上次自动更新 <b data-tip={F.dtFull(d.fetched_at)}>{F.dt(d.fetched_at)}</b>（{F.ago(d.fetched_at)}，数据日期 {d.rates_date ?? '—'}）</> : '还没有成功更新过'}
                {' · '}{d.auto ? '每天自动更新' : '自动更新已在配置里关闭（[fx] auto = false）'} <InfoTip text={`汇率源：${d.url}`} />
              </div>
              {d.last_error && <Note tone="warn">最近一次更新失败（{F.dt(d.last_attempt_at)}），沿用上次的汇率 <InfoTip text={d.last_error} /></Note>}
              {d.missing.length > 0 && <Note tone="warn">这些币种在用但没有汇率：{d.missing.join('、')}。请手动设置，否则费用不会折成美元</Note>}
            </div>
            <TableWrap>
              <table className="dt" style={{ minWidth: 720 }}>
                <colgroup><col style={{ width: 90 }} /><col style={{ width: 200 }} /><col style={{ width: 170 }} /><col style={{ width: 110 }} /><col style={{ width: 70 }} /><col /></colgroup>
                <thead><tr><th>币种</th><th>汇率</th><th>反向</th><th>来源</th><th>状态</th><th /></tr></thead>
                <tbody>
                  {rows.map((r) => (
                    <tr key={r.currency}>
                      <td className="strong mono">{r.currency}</td>
                      <td className="num">{r.usd_per_unit != null ? F.fxRate(r.currency, r.usd_per_unit) : <span className="t-muted">未折算</span>}</td>
                      <td className="num t-sub">{r.per_usd != null ? `1 USD = ${n4(r.per_usd)} ${r.currency}` : '—'}</td>
                      <td><SourcePill r={r} date={d.rates_date} /></td>
                      <td>{r.in_use ? <span className="t-muted">—</span> : <span className="t-muted">未在用</span>}</td>
                      <td className="r">{r.source !== 'base' && <span className="row row-acts" style={{ justifyContent: 'flex-end', flexWrap: 'nowrap' }}>
                        <Button size="sm" variant="quiet" onClick={() => setEdit({ currency: r.currency })}><Pencil size={13} />手动设置</Button>
                        {r.source === 'manual' && <Button size="sm" variant="quiet" onClick={() => setAsk(r.currency)}><RotateCcw size={13} />改回自动</Button>}
                      </span>}</td>
                    </tr>
                  ))}
                </tbody>
              </table>
              <ShowAll total={main.length + rest.length} cap={main.length} open={all} onToggle={() => setAll((v) => !v)} unit="个币种" />
            </TableWrap>
          </>
        )}
      </Panel>
      {editHeld && <RateDialog target={editHeld} data={d} open={edit != null} onClose={() => setEdit(null)} />}
      <ConfirmDialog open={!!ask} onOpenChange={(o) => !o && setAsk(null)} title={`改回自动汇率 · ${askHeld ?? ''}`} confirmLabel="改回自动"
        description={(() => { const r = d?.rates.find((x) => x.currency === askHeld); return r?.auto_per_usd != null ? `改回自动汇率：1 USD = ${n4(r.auto_per_usd)} ${askHeld}` : `自动汇率里没有 ${askHeld}，改回后这个币种的费用不会折成美元`; })()}
        onConfirm={async () => { await clear.mutateAsync(ask!); toast.ok('已改回自动汇率'); }} />
    </>
  );
}

/** 手动设置一个币种的汇率：两种写法任选（1 X = ? USD，或 1 USD = ? X），提交时带对应的字段 */
const RateDialog = resetOnOpen(function RateDialog({ target, data, open, onClose }: { target: { currency: string } | 'new'; data: FxRates | undefined; open: boolean; onClose: () => void }) {
  const set = useSetFxRate();
  const [cur, setCur] = React.useState(target === 'new' ? '' : target.currency);
  const row = data?.rates.find((r) => r.currency === cur.trim().toUpperCase());
  type Mode = 'per_usd' | 'usd_per_unit';
  const [mode, setMode] = React.useState<Mode>('per_usd');
  const [v, setV] = React.useState(() => (row?.per_usd != null ? n4(row.per_usd) : ''));
  // 用户亲手填的那个值与当时的写法：切写法时从它换算（来回切不累积误差），切回原写法就恢复原值
  const typed = React.useRef({ mode, v });
  const switchMode = (m: Mode) => {
    const o = typed.current; const on = Number(o.v);
    if (m !== mode && o.v.trim() !== '' && isFinite(on) && on > 0) setV(m === o.mode ? o.v : String(Number((1 / on).toPrecision(6))));
    setMode(m);
  };
  const c = cur.trim().toUpperCase();
  const num = Number(v);
  const curOk = /^[A-Z]{3}$/.test(c) && c !== 'USD';
  const valOk = v.trim() !== '' && isFinite(num) && num > 0;
  const usd = valOk ? (mode === 'per_usd' ? 1 / num : num) : null;
  const submit = () => set.mutate({ currency: c, [mode]: num }, { onSuccess: () => { toast.ok(`已手动设置 ${c} 的汇率`); onClose(); }, onError: toast.err });
  return (
    <FormDialog size="md" open={open} onOpenChange={(o) => !o && onClose()} title={target === 'new' ? '手动设置汇率' : `手动设置汇率 · ${c}`} dirty={v.trim() !== ''}
      desc={<>设置后不再被自动更新覆盖 <InfoTip text="想用回自动汇率，在表里点「改回自动」" /></>} submitLabel="保存" busy={set.isPending} valid={curOk && valOk} onSubmit={submit}>
      <div className="form-grid">
        {target === 'new' && <Field label="币种" error={cur.trim() && !curOk ? (c === 'USD' ? 'USD 恒为 1' : '3 个字母的代码，如 SGD') : undefined}>
          <Input value={cur} onChange={(e) => setCur(e.target.value.toUpperCase())} maxLength={3} placeholder="如 SGD" className="mono" autoFocus /></Field>}
        <Field label="写法"><Segmented value={mode} onChange={switchMode} label="汇率的写法" items={[{ value: 'per_usd', label: `1 USD = ? ${c || '币种'}` }, { value: 'usd_per_unit', label: `1 ${c || '币种'} = ? USD` }]} /></Field>
        <Field label="汇率" error={v.trim() && !valOk ? '填一个正数' : undefined}
          hint={usd != null && curOk ? `约合 ${F.fxRate(c, usd)}${row?.auto_per_usd != null ? `；自动汇率是 1 USD = ${n4(row.auto_per_usd)} ${c}` : ''}` : row?.auto_per_usd != null ? `自动汇率是 1 USD = ${n4(row.auto_per_usd)} ${c}` : undefined}>
          <Input inputMode="decimal" value={v} onChange={(e) => { const nv = e.target.value.replace(/[^\d.]/g, ''); setV(nv); typed.current = { mode, v: nv }; }} placeholder={mode === 'per_usd' ? '7.12' : '0.1404'} /></Field>
      </div>
    </FormDialog>
  );
});
