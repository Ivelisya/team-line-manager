// 测速作业的步骤结果（作业详情）与一句话摘要（抽屉「管理」里的测速行）：平均速率、耗时、首字节，没下完时说明。
// 速率只算收到第一个字节之后的（首字节时间含 DNS、建连和 TLS 握手，单列）；没下完 = curl 300 秒的时限到了，速率按已下载的那部分算。
import type { SpeedtestOutput } from '@/lib/types';
import * as F from '@/lib/format';

/** 步骤输出是不是测速结果（有 avg_mbps） */
export const isSpeedtestOutput = (o: Record<string, unknown> | null | undefined): o is Record<string, unknown> & SpeedtestOutput => !!o && typeof o.avg_mbps === 'number';

const secs = (n: number) => `${n >= 100 ? Math.round(n) : n.toFixed(1)} 秒`;
const ttfb = (s: number) => `${Math.round(s * 1000)} 毫秒`;
const rate = (o: SpeedtestOutput) => F.str(F.mbps(o.avg_mbps));
/** 没下完的说明：300 秒内只下了多少、计划多少 */
const partial = (o: SpeedtestOutput) => `没下完：${Math.round(o.total_s)} 秒内只下载了 ${F.str(F.bytes(o.bytes))}（计划 ${o.size_mb} MB），速率按已下载的算，慢线路建议换小一点的下载量`;

/** 一句话：「86.4 Mbps（10.8 MB/s）· 耗时 10.5 秒 · 首字节 500 毫秒 · 圣何塞 100 MB」，没下完的末尾标一句 */
export const speedtestLine = (o: SpeedtestOutput) => `${rate(o)}（${o.avg_mb_s} MB/s）· 耗时 ${secs(o.total_s)} · 首字节 ${ttfb(o.ttfb_s)} · ${o.target_name} ${o.size_mb} MB${o.complete ? '' : ' · 没下完'}`;

export function SpeedtestResult({ out }: { out: SpeedtestOutput }) {
  return (
    <div className="tune-res">
      <div className="speed-main"><b className="num">{F.mbps(out.avg_mbps).v}</b><span>{F.mbps(out.avg_mbps).u}</span><span className="t-sub">平均速率 · {out.avg_mb_s} MB/s</span></div>
      <div className="tune-sum">耗时 <b className="num">{secs(out.total_s)}</b> · 首字节 <b className="num">{ttfb(out.ttfb_s)}</b> · 下载 <b className="num">{F.str(F.bytes(out.bytes))}</b></div>
      <div className="tune-sum">从{out.target_name}的测速主机{out.remote_ip && <>（<span className="mono">{out.remote_ip}</span>）</>}下载，走这台机器自己的出站路径</div>
      {!out.complete && <div className="tune-sum tune-warn">{partial(out)}</div>}
    </div>
  );
}
