// 系统调优作业的步骤结果：摘要 + 推导参数一行 + 「参数 / 结果 / 说明」表（只列写入的、跳过的、保留现值的；已是推荐值的收成一行，点开才列）
// + 第三方调优残留 + 其余几项写成一行中文（不再把 JSON 原样输出）。撤销调优（rollback）的结果同一个组件，用词换成「写回原值」。
import * as React from 'react';
import { InfoTip } from '@/components/ui/form';
import { mem, str } from '@/lib/format';
import type { TuneParams } from '@/lib/types';

type Skipped = { key: string; why: string; got: string | null };
type Row = { key: string; result: string; note: string };

const arr = (v: unknown): string[] => (Array.isArray(v) ? v.map(String) : []);

/** 步骤输出是不是调优结果（有 applied 数组） */
export const isTuneOutput = (o: Record<string, unknown> | null | undefined): o is Record<string, unknown> => !!o && Array.isArray(o.applied);

/** 推导参数一句话：「缓冲上限 43.0 MiB ← 带宽 1000 Mbps（套餐 2560，按 1000 算）· RTT 172 ms（入口连接 25 条的 P90）· 内存 3.8 GB」 */
export function tuneParamsText(p: TuneParams): string {
  const bw = p.bw_source === 'plan' ? (p.bw_plan != null && p.bw_plan !== p.bw ? `套餐 ${p.bw_plan}，按 ${p.bw} 算` : '套餐') : '套餐没写带宽，按默认';
  const rtt = p.rtt_source === 'sampled' ? `入口连接 ${p.rtt_samples ?? '?'} 条的 P90` : (p.rtt_samples ?? 0) > 0 ? `样本 ${p.rtt_samples} 条不够，按默认` : '默认';
  const m = p.mem_mb != null ? ` · 内存 ${str(mem(p.mem_mb * 1024 * 1024))}` : '';
  return `缓冲上限 ${(p.buf_max / 1048576).toFixed(1)} MiB ← 带宽 ${p.bw} Mbps（${bw}）· RTT ${p.rtt} ms（${rtt}）${m}`;
}

export function TuneResult({ out }: { out: Record<string, unknown> }) {
  const rollback = out.rollback === true;
  const applied = arr(out.applied), ok = arr(out.ok), kept = arr(out.kept);
  const skipped = (Array.isArray(out.skipped) ? out.skipped : []) as Skipped[];
  const before = (out.before && typeof out.before === 'object' ? out.before : {}) as Record<string, string>;
  const warnings = arr(out.warnings);
  const params = out.params && typeof out.params === 'object' ? (out.params as TuneParams) : null;
  const route = out.route && typeof out.route === 'object' ? (out.route as { initcwnd: number | null; initrwnd: number | null }) : null;
  const [more, setMore] = React.useState(false);
  const [doneLabel, okLabel] = rollback ? ['已写回原值', '本来就是原值'] : ['已写入', '已是推荐值'];
  const rows: Row[] = [
    ...applied.map((key) => ({ key, result: doneLabel, note: key in before ? `原来 ${before[key] || '（空）'}` : '' })),
    ...skipped.map((s) => ({ key: s.key, result: '跳过', note: s.why === 'readonly' ? `只读${s.got ? `（当前 ${s.got}）` : ''}` : '内核没有这一项' })),
    ...kept.map((key) => ({ key, result: '保留现值', note: '现值比推荐值更大' })),
    ...(more ? ok.map((key) => ({ key, result: okLabel, note: '' })) : []),
  ];
  const sum = ([[doneLabel, applied.length], [okLabel, ok.length], ['保留现值', kept.length], ['跳过', skipped.length]] as const).filter(([, n]) => n > 0);
  const container = out.container === true;
  const ntp = out.ntp === 'synced' ? '时间同步正常' : out.ntp === 'unsynced' ? '时间还没同步' : out.ntp === 'container' ? '时间同步由宿主机负责' : '';
  const snapshot = out.snapshot === 'created' ? '已记下调优前的原值（可撤销）' : out.snapshot === 'created_tuned' ? '已记下原值（之前调过，记的是旧档的值）' : out.snapshot === 'exists' ? '原值快照已在' : '';
  const extra = [
    container && '容器内',
    out.bbr != null && `BBR ${out.bbr ? '已启用' : '未启用'}`,
    ntp,
    out.journald === 'changed' ? 'journald 已限额' : out.journald === 'same' ? 'journald 限额已是最新' : '',
    out.nofile ? `文件句柄 ${String(out.nofile)}` : '',
    out.swap_mb != null && `交换 ${str(mem(Number(out.swap_mb) * 1024 * 1024))}`,
    out.mem_mb != null && !params && `内存 ${str(mem(Number(out.mem_mb) * 1024 * 1024))}`,
    snapshot,
    route && `默认路由 initcwnd ${route.initcwnd ?? '未设'} / initrwnd ${route.initrwnd ?? '未设'}（不是本档设的，不动）`,
  ].filter(Boolean);
  const tip = rollback
    ? '已写回原值：按调优前记下的原值写回\n本来就是原值：现值就是原值，没有动\n跳过：内核没有这一项，或容器内只读'
    : '已写入：这次写入了推荐值（说明里是原来的值）\n已是推荐值：现值本来就是，没有动\n保留现值：现值比推荐值更大，不往小改\n跳过：内核没有这一项，或容器内只读';
  return (
    <div className="tune-res">
      <div className="tune-sum">{rollback && '撤销调优：'}{sum.length ? sum.map(([label, n], i) => <React.Fragment key={label}>{i > 0 && ' · '}{label} <b className="num">{n}</b> 项</React.Fragment>) : '没有参数需要调整'}
        <InfoTip text={tip} /></div>
      {params && <div className="tune-sum">{tuneParamsText(params)}
        <InfoTip text={'缓冲上限 = 2 × 带宽 × RTT ÷ 8 + 2 MiB，不低于 16 MiB，不高于 内存 ÷ 32 与 64 MiB 中较小的。\n带宽取套餐（封顶 1000 Mbps），RTT 线路机取客户入口连接的 P90（夹在 150–300 ms），节点固定 100 ms。'} /></div>}
      {rows.length > 0 && (
        <table className="tune-tbl">
          <thead><tr><th>参数</th><th>结果</th><th>说明</th></tr></thead>
          <tbody>{rows.map((r) => <tr key={r.key}><td className="mono">{r.key}</td><td>{r.result}</td><td>{r.note}</td></tr>)}</tbody>
        </table>
      )}
      {ok.length > 0 && <button type="button" className="btn btn-link btn-sm tune-more" aria-expanded={more} onClick={() => setMore((v) => !v)}>{more ? '收起' : `另 ${ok.length} 项${okLabel}，展开`}</button>}
      {warnings.map((w) => <div key={w} className="tune-sum tune-warn">{w}</div>)}
      {extra.length > 0 && <div className="tune-sum">{extra.join(' · ')}</div>}
    </div>
  );
}
