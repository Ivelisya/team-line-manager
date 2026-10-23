// 来源 IP 画像的小部件：接入方式徽标、运营商·城市单元格、IP 库状态、节点抽屉里的「来源画像」卡。
// 数据来自 /machines/{id}/sources 与 /ipintel/status（见 lib/ipintel.ts）；来源行上的 intel 字段来自 /access/sources。
import { ShieldQuestion } from 'lucide-react';
import { useIpIntelStatus, useSourcesProfile, type ConnType, type DistItem, type IpIntel, type IpIntelFile, type ProfileWindow, type SourcesProfile } from '@/lib/ipintel';
import { countryName } from '@/lib/flags';
import * as F from '@/lib/format';
import { InfoTip } from '@/components/ui/form';
import { Chip, ErrorNote, Note, Pill, Section, SectionHeader, type Tone } from '@/components/app/primitives';
import '@/styles/access.css';

const TONE: Record<ConnType, Tone> = { residential: 'ok', mobile: 'info', datacenter: 'warn', vpn: 'bad', unknown: 'neutral' };
const CONF: Record<string, string> = { high: '高', medium: '中', low: '低' };
const INSTALL_CMD = '在中控运行 lab/install-ipintel.sh 安装';

/** 境外地点「US · Dallas」的国家代码换成中文国名「美国 · Dallas」（国内的本来就是中文「河南省郑州市」） */
export const placeZh = (place: string) => place.replace(/^([A-Z]{2})(?= · |$)/, (cc) => countryName(cc));

/** 接入方式徽标：住宅 / 移动 / 机房 / VPN / 未知各一个色调，悬停看判定理由 */
function ConnPill({ intel }: { intel: Pick<IpIntel, 'conn_type' | 'conn_label' | 'confidence' | 'reason'> }) {
  return <Pill tone={TONE[intel.conn_type] ?? 'neutral'} tip={`${intel.reason}（置信度${CONF[intel.confidence] ?? intel.confidence}）`}>{intel.conn_label}</Pill>;
}

/** 来源行的「运营商 · 城市」：放不下时省略，悬停看全文（含 ASN）；查不到给一个破折号 */
export function IspPlace({ intel }: { intel: IpIntel | null | undefined }) {
  if (!intel) return <span className="t-muted">—</span>;
  const place = placeZh(intel.place);
  return (
    <span className="ellipsis block" data-tip={[intel.isp, place, intel.asn ? `AS${intel.asn} ${intel.as_org ?? ''}` : ''].filter(Boolean).join(' · ')}>
      <span className="strong">{intel.isp}</span>{place && <span className="t-sub"> · {place}</span>}
    </span>
  );
}

/** 来源行的接入方式：我方机器和非公网地址不判断接入方式 */
export function ConnCell({ intel }: { intel: IpIntel | null | undefined }) {
  if (!intel) return <span className="t-muted">—</span>;
  if (intel.internal) return <Chip variant="neutral" tip={`自有机器：${intel.internal}`}>自有 · {intel.internal}</Chip>;
  if (intel.private) return <Chip variant="neutral">非公网</Chip>;
  return <ConnPill intel={intel} />;
}

export const CONN_FILTER_OPTIONS = [
  { value: 'residential', label: '住宅宽带' }, { value: 'mobile', label: '移动网络' }, { value: 'datacenter', label: '机房' }, { value: 'vpn', label: 'VPN / 代理' }, { value: 'unknown', label: '未知' },
];

/** 各个库的中文名（status 里的 id 是内部名，不显示） */
const DB_NAME: Record<string, string> = {
  ip2region_v4: 'IPv4 归属地库', ip2region_v6: 'IPv6 归属地库', dbip_city: '城市库（DB-IP）', dbip_asn: '运营商库（DB-IP）', iptoasn: 'ASN 库（iptoasn）',
};
/** 库的更新日期「10-20 更新」：xdb、DB-IP 的 build 是构建时刻（Unix 秒），iptoasn 的 build 是行数，用文件修改时间 */
function dbDate(f: IpIntelFile): string {
  const secs = f.build && /^\d{9,}$/.test(f.build) ? Number(f.build) : f.mtime;
  return secs ? `${F.dt(new Date(secs * 1000).toISOString()).slice(0, 5)} 更新` : '';
}
const dbLine = (f: IpIntelFile) => `${DB_NAME[f.id] ?? '其他库'}：${f.loaded ? dbDate(f) || '已加载' : '未加载'}`;

/** IP 库状态：访问监控页右上角的小徽标；悬停看各库明细 */
export function IpIntelStatusChip() {
  const q = useIpIntelStatus();
  const s = q.data;
  if (!s) return null;
  if (!s.enabled) return <Pill tone="neutral" tip="IP 库已在中控配置里关闭">IP 库已关闭</Pill>;
  if (!s.installed) return <Pill tone="warn" tip={INSTALL_CMD}>IP 库未安装</Pill>;
  const ready = s.loaded >= s.expected;
  return <Pill tone={ready ? 'ok' : 'warn'} tip={s.files.map(dbLine).join('\n')}>{ready ? 'IP 库已就绪' : `IP 库不全 ${s.loaded}/${s.expected}`}</Pill>;
}

/** 库没装好时的提示条（来源地址页签用）：短字 + (i) 里的原因、怎么处理 */
export function IpIntelHint() {
  const s = useIpIntelStatus().data;
  if (!s || (s.enabled && s.loaded >= s.expected)) return null;
  const [text, why] = !s.enabled ? ['IP 库已关闭', '不显示运营商与城市；在中控配置里打开'] : !s.installed ? ['IP 库未安装', INSTALL_CMD]
    : ['IP 库不全', `部分来源查不到运营商和城市。缺少：${s.files.filter((f) => !f.loaded).map(dbLine).join('，')}\n${INSTALL_CMD}`];
  return <Note tone="warn" icon={<ShieldQuestion size={16} />}>{text} <InfoTip text={why} /></Note>;
}

// ── 节点抽屉：来源画像卡 ────────────────────────────────────────────────────

const DIST_ROW = { gridTemplateColumns: 'minmax(0, 1fr) auto', rowGap: 3 };
function Dist({ title, items, empty = '没有数据' }: { title: string; items: DistItem[]; empty?: string }) {
  const top = items.slice(0, 5);
  return (
    <div>
      <div className="hint" style={{ marginBottom: 6 }}>{title}</div>
      {top.length === 0 ? <div className="hint">{empty}</div> : (
        <div className="dist">
          {top.map((x) => (
            // 名字与占比一行、条在下面占满整宽：三列并排时名字只分到四成宽，「Bharat Sanchar Nigam」这类运营商名放不下。悬停整行看全文
            <div key={x.value} className="dist-row" style={DIST_ROW} data-tip={`${placeZh(x.label)}：${x.conns.toLocaleString('zh-CN')} 次连接，${x.sources} 个来源`}>
              <span className="dist-name">{placeZh(x.label)}</span>
              <span className="dist-pct">{x.pct.toFixed(x.pct < 10 ? 1 : 0)}%</span>
              <span className="dist-track" style={{ gridColumn: '1 / -1' }}><span className="dist-fill" style={{ width: `${Math.max(2, Math.min(100, x.pct))}%` }} /></span>
            </div>
          ))}
          {items.length > top.length && <div className="hint">另有 {items.length - top.length} 项</div>}
        </div>
      )}
    </div>
  );
}

function learningText(l: SourcesProfile['learning']): string {
  if (!l.started_at) return '还没有来源记录';
  if (l.done) return `学习期已结束（${l.learn_days} 天）`;
  const h = l.remaining_hours ?? 0;
  return `学习期还剩 ${h >= 24 ? `${Math.floor(h / 24)} 天 ${h % 24} 小时` : `${h} 小时`}`;
}

export function SourceProfile({ id, window, relay }: { id: string; window: ProfileWindow; relay: boolean }) {
  const q = useSourcesProfile(id, window);
  const d = q.data;
  if (q.isError && !d) return <Section><SectionHeader title="来源画像" /><ErrorNote error={q.error} onRetry={() => void q.refetch()} /></Section>;
  if (!d) return <Section><SectionHeader title="来源画像" /><div className="skel" style={{ height: 96 }} /></Section>;
  const known = d.known.isp.length + d.known.city.length;
  const outside = d.internal_conns + d.private_conns;
  // 学习期说明、已学基线、没计入的我方 / 内网连接都放进同一个 (i)
  const learnTip = [
    `前 ${d.learning.learn_days} 天是学习期，只记录不告警。之后出现新的运营商、城市或机房来源，会告警`,
    known > 0 ? `已记住 ${d.known.isp.length} 个运营商、${d.known.city.length} 个城市。` : '',
    outside > 0 ? `另有 ${outside.toLocaleString('zh-CN')} 次连接来自自有机器或内网，没有计入` : '',
  ].filter(Boolean).join('\n');
  const span = window === '24h' ? '24 小时' : window === '7d' ? '7 天' : '30 天';
  return (
    <Section>
      <SectionHeader title="来源画像" meta={<>近 {span} · {d.total_sources} 个来源{relay && <InfoTip text="汇总这台线路机名下所有节点的外部来源" />}</>} />
      {/* 换时间范围、新数据未到时沿用的旧画像调淡（components.css [data-stale]） */}
      <div data-stale={q.isPlaceholderData}>
        {d.degraded && <Note tone="warn" icon={<ShieldQuestion size={16} />}>IP 库未安装 <InfoTip text={INSTALL_CMD} /></Note>}
        {d.empty ? <div className="hint">这段时间没有来源记录</div> : d.total_conns === 0 && !d.degraded ? <div className="hint">只有自有机器或内网的连接</div> : (
          <>
            <div className="dist-grid">
              <Dist title="运营商" items={d.distribution.isp} empty={d.degraded ? '查不到' : undefined} />
              <Dist title="城市" items={d.distribution.city} empty={d.degraded ? '查不到' : undefined} />
              <Dist title="接入方式" items={d.distribution.conn_type} empty={d.degraded ? '查不到' : undefined} />
            </div>
            <div className="hint" style={{ marginTop: 8 }}>{learningText(d.learning)} <InfoTip text={learnTip} /></div>
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
