// 来源 IP 画像的小部件：接入方式徽标、运营商·城市单元格、IP 库状态、节点抽屉里的「来源画像」卡。
// 数据来自 /machines/{id}/sources 与 /ipintel/status（见 lib/ipintel.ts）；来源行上的 intel 字段来自 /access/sources。
import { ShieldQuestion } from 'lucide-react';
import { useIpIntelStatus, useSourcesProfile, type ConnType, type DistItem, type IpIntel, type ProfileWindow, type SourcesProfile } from '@/lib/ipintel';
import * as F from '@/lib/format';
import { Chip, ErrorNote, Note, Pill, Section, SectionHeader, type Tone } from '@/components/app/primitives';

const TONE: Record<ConnType, Tone> = { residential: 'ok', mobile: 'info', datacenter: 'warn', vpn: 'bad', unknown: 'neutral' };
const CONF: Record<string, string> = { high: '高', medium: '中', low: '低' };
const INSTALL_HINT = 'IP 库未安装：在中控运行 lab/install-ipintel.sh';

/** 接入方式徽标：住宅 / 移动 / 机房 / VPN / 未知各一个色调，悬停看判定理由 */
function ConnPill({ intel }: { intel: Pick<IpIntel, 'conn_type' | 'conn_label' | 'confidence' | 'reason'> }) {
  return <Pill tone={TONE[intel.conn_type] ?? 'neutral'} tip={`${intel.reason}（置信度${CONF[intel.confidence] ?? intel.confidence}）`}>{intel.conn_label}</Pill>;
}

/** 来源行的「运营商 · 城市」；查不到给一个破折号 */
export function IspPlace({ intel }: { intel: IpIntel | null | undefined }) {
  if (!intel) return <span className="t-muted">—</span>;
  return (
    <span className="ellipsis" data-tip={[intel.isp, intel.place, intel.asn ? `AS${intel.asn} ${intel.as_org ?? ''}` : ''].filter(Boolean).join(' · ')}>
      <span className="strong">{intel.isp}</span>{intel.place && <span className="t-sub"> · {intel.place}</span>}
    </span>
  );
}

/** 来源行的接入方式：我方机器和非公网地址不判断接入方式 */
export function ConnCell({ intel }: { intel: IpIntel | null | undefined }) {
  if (!intel) return <span className="t-muted">—</span>;
  if (intel.internal) return <Chip variant="neutral" tip={`我方机器：${intel.internal}`}>我方 · {intel.internal}</Chip>;
  if (intel.private) return <Chip variant="neutral">非公网</Chip>;
  return <ConnPill intel={intel} />;
}

export const CONN_FILTER_OPTIONS = [
  { value: 'residential', label: '住宅宽带' }, { value: 'mobile', label: '移动网络' }, { value: 'datacenter', label: '机房' }, { value: 'vpn', label: 'VPN·代理' }, { value: 'unknown', label: '未知 / 查不到' },
];
/** 来源行按接入方式筛：unknown 同时包含查不到画像的行 */
export const connMatches = (intel: IpIntel | null | undefined, want: string) => !want || (intel ? intel.conn_type === want : want === 'unknown');

/** IP 库状态：访问监控页右上角的小徽标；悬停看各库明细 */
export function IpIntelStatusChip() {
  const q = useIpIntelStatus();
  const s = q.data;
  if (!s) return null;
  if (!s.enabled) return <Pill tone="neutral" tip="IP 情报已在配置里关闭（[ipintel] enabled = false）">IP 库已关闭</Pill>;
  if (!s.installed) return <Pill tone="warn" tip={s.hint ?? INSTALL_HINT}>IP 库未安装</Pill>;
  const detail = s.files.map((f) => `${f.id}：${f.loaded ? `已加载${f.build ? `（${f.build}）` : ''}` : '未加载'}`).join('\n');
  return <Pill tone={s.loaded >= s.expected ? 'ok' : 'warn'} tip={detail}>IP 库 {s.loaded}/{s.expected}</Pill>;
}

/** 库没装好时的提示条（来源地址页签用） */
export function IpIntelHint() {
  const s = useIpIntelStatus().data;
  if (!s || (s.enabled && s.loaded >= s.expected)) return null;
  const text = !s.enabled ? 'IP 情报已在配置里关闭，来源地址不显示运营商、城市和接入方式。' : !s.installed ? INSTALL_HINT : (s.hint ?? 'IP 库不全，部分来源的画像查不到。');
  return <Note tone="warn" icon={<ShieldQuestion size={16} />}>{text}</Note>;
}

// ── 节点抽屉：来源画像卡 ────────────────────────────────────────────────────

function Dist({ title, items, empty = '没有数据' }: { title: string; items: DistItem[]; empty?: string }) {
  const top = items.slice(0, 5);
  return (
    <div>
      <div className="hint" style={{ marginBottom: 6 }}>{title}</div>
      {top.length === 0 ? <div className="hint">{empty}</div> : (
        <div className="dist">
          {top.map((x) => (
            <div key={x.value} className="dist-row" data-tip={`${x.label}：${x.conns.toLocaleString('zh-CN')} 次连接，${x.sources} 个来源`}>
              <span className="dist-name">{x.label}</span>
              <span className="dist-track"><span className="dist-fill" style={{ width: `${Math.max(2, Math.min(100, x.pct))}%` }} /></span>
              <span className="dist-pct">{x.pct.toFixed(x.pct < 10 ? 1 : 0)}%</span>
            </div>
          ))}
          {items.length > top.length && <div className="hint">另有 {items.length - top.length} 项</div>}
        </div>
      )}
    </div>
  );
}

function learningText(l: SourcesProfile['learning']): string {
  if (!l.started_at) return '还没有来源记录，学习期从第一条记录开始算';
  if (l.done) return `学习期已结束（${l.learn_days} 天）· 之后出现的新运营商、新城市、机房来源会告警`;
  const h = l.remaining_hours ?? 0;
  const left = h >= 24 ? `${Math.floor(h / 24)} 天 ${h % 24} 小时` : `${h} 小时`;
  return `学习期还剩 ${left}（共 ${l.learn_days} 天）· 期间只学习不告警`;
}

export function SourceProfile({ id, window, relay }: { id: string; window: ProfileWindow; relay: boolean }) {
  const q = useSourcesProfile(id, window);
  const d = q.data;
  if (q.isError && !d) return <Section><SectionHeader title="来源画像" /><ErrorNote error={q.error} onRetry={() => void q.refetch()} /></Section>;
  if (!d) return <Section><SectionHeader title="来源画像" /><div className="skel" style={{ height: 96 }} /></Section>;
  const known = d.known.isp.length + d.known.city.length;
  return (
    <Section>
      <SectionHeader title="来源画像" meta={`${relay ? '汇总名下所有节点 · ' : ''}最近 ${window === '24h' ? '24 小时' : window === '7d' ? '7 天' : '30 天'} · ${d.total_sources} 个外部来源`} />
      {/* 换时间范围、新数据未到时沿用的旧画像调淡（extras.css [data-stale]） */}
      <div data-stale={q.isPlaceholderData}>
        {d.degraded && <Note tone="warn" icon={<ShieldQuestion size={16} />}>{INSTALL_HINT}</Note>}
        {d.empty ? <div className="hint">这段时间没有来源记录</div> : d.total_conns === 0 && !d.degraded ? <div className="hint">这段时间只有我方机器或非公网地址的连接，没有外部来源可画像</div> : (
          <>
            <div className="dist-grid">
              <Dist title="运营商" items={d.distribution.isp} empty={d.degraded ? '查不到（IP 库未安装）' : undefined} />
              <Dist title="城市" items={d.distribution.city} empty={d.degraded ? '查不到（IP 库未安装）' : undefined} />
              <Dist title="接入方式" items={d.distribution.conn_type} empty={d.degraded ? '查不到（IP 库未安装）' : undefined} />
            </div>
            <div className="hint" style={{ marginTop: 8 }}>
              {learningText(d.learning)}
              {known > 0 && <> · 已学基线：{d.known.isp.length} 个运营商、{d.known.city.length} 个城市</>}
              {(d.internal_conns > 0 || d.private_conns > 0) && <> · 另有 {(d.internal_conns + d.private_conns).toLocaleString('zh-CN')} 次连接来自我方机器或非公网地址，未计入分布</>}
            </div>
            {d.known.isp.some((k) => k.anomaly) && (
              <div className="row" style={{ marginTop: 6, gap: 4 }}>
                <span className="hint">新出现的运营商：</span>
                {d.known.isp.filter((k) => k.anomaly).map((k) => <Chip key={k.value} variant="warn" tip={`首次出现 ${F.dtFull(k.first_seen)}`}>{k.label}</Chip>)}
              </div>
            )}
          </>
        )}
        {d.anomalies.map((a) => <div key={a.id} style={{ marginTop: 8 }}><Note tone="warn"><strong>{a.rule_name}</strong>：{a.summary.replace(/^[^：]*：/, '')}</Note></div>)}
      </div>
    </Section>
  );
}
