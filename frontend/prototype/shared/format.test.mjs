// Run: ~/.local/bin/node --test   (from prototype/shared)
import test from 'node:test';
import assert from 'node:assert/strict';
import * as f from './format.js';

const KiB = 1024, MiB = KiB ** 2, GiB = KiB ** 3, TiB = KiB ** 4;
const AS_OF = 1790148138; // 2026-09-23 15:22:18 北京时间 (snapshot)
const t = (x) => String(x);

test('§4.2 bytes: IEC units, ≥1000 steps up, 3 significant digits', () => {
  assert.equal(t(f.bytes(0)), '0 B');
  assert.equal(f.bytes(0).kind, 'zero');
  assert.equal(t(f.bytes(812)), '812 B');
  assert.equal(t(f.bytes(1.23 * KiB)), '1.23 KiB');
  assert.equal(t(f.bytes(12.3 * MiB)), '12.3 MiB');
  assert.equal(t(f.bytes(123 * GiB)), '123 GiB');
  assert.equal(t(f.bytes(1000 * MiB)), '0.98 GiB');
  assert.equal(t(f.bytes(1.07 * TiB)), '1.07 TiB');
  assert.equal(t(f.bytes(1000)), '0.98 KiB'); // 1000 B is already ≥ 1000
  assert.equal(t(f.bytes(999.6)), '0.98 KiB'); // rounding up to 1000 B steps up too
  assert.equal(t(f.bytes(9.996 * KiB)), '10.0 KiB');
  assert.equal(t(f.bytes(99.96 * KiB)), '100 KiB');
  assert.equal(t(f.bytes(999.7 * MiB)), '0.98 GiB');
});

test('§4.2 bytes: real snapshot figures, parts and exact tooltip', () => {
  assert.equal(t(f.bytes(41708937291)), '38.8 GiB'); // kpis.today_business.total
  assert.equal(t(f.bytes(1159869537661)), '1.05 TiB'); // kpis.month_business.total
  const b = f.bytes(1318123456);
  assert.equal(b.num, '1.23');
  assert.equal(b.unit, 'GiB');
  assert.equal(b.exact, '1,318,123,456 B');
  assert.equal(f.bytesPair(10 * GiB, 1 * GiB).text, '10.0 GiB 1.00 GiB');
});

test('§4.9 missing vs zero', () => {
  for (const v of [null, undefined, NaN]) {
    assert.equal(t(f.bytes(v)), '—');
    assert.equal(f.bytes(v).kind, 'missing');
    assert.equal(f.bytes(v).tone, 'muted');
  }
  assert.equal(t(f.rate(0)), '0 bps');
  assert.equal(t(f.unset()), '未设置');
  assert.equal(t(f.unlimited()), '不限');
  assert.equal(f.unlimited().tone, 'secondary');
  assert.equal(t(f.pending()), '待定');
  assert.equal(f.pending().tone, 'warn');
  assert.equal(t(f.na('无主机')), '无主机');
  assert.equal(t(f.loadFailed()), '读取失败');
  const u = f.unverified(f.bytes(5 * GiB));
  assert.equal(t(u), '—');
  assert.equal(u.chip, '待核验');
  assert.match(u.exact, /5\.00 GiB/);
  const s = f.stale(f.rate(12.4e6), 180);
  assert.equal(t(s), '12.4 Mbps');
  assert.equal(s.kind, 'stale');
  assert.equal(s.hollow, true);
  assert.equal(s.exact, '3 分钟前的数据');
});

test('§4.3 rates: decimal bits per second', () => {
  assert.equal(t(f.rate(842e3)), '842 Kbps');
  assert.equal(t(f.rate(1.1e6)), '1.10 Mbps');
  assert.equal(t(f.rate(12.4e6)), '12.4 Mbps');
  assert.equal(t(f.rate(128e6)), '128 Mbps');
  assert.equal(t(f.rate(1.02e9)), '1.02 Gbps');
  assert.equal(t(f.rate(999.6e3)), '1.00 Mbps');
  assert.equal(t(f.rate(500)), '<1 Kbps');
  assert.equal(t(f.rate(999)), '<1 Kbps');
  assert.equal(t(f.rate(1000)), '1.00 Kbps');
  assert.equal(t(f.rate(9117730)), '9.12 Mbps'); // kpis.business_rate.down_bps
  assert.equal(t(f.rate(3298)), '3.30 Kbps'); // 团队1-31 rate.down_bps
  assert.equal(t(f.rate(12.4e6, { dir: 'down' })), '↓ 12.4 Mbps');
  assert.equal(f.ratePair(12.4e6, 1.1e6).text, '↓ 12.4 Mbps ↑ 1.10 Mbps');
  assert.equal(f.rate(12.4e6).exact, '12,400,000 bit/s（1.48 MiB/s）');
});

test('§4.3 rates: MiB/s mode is selectable', () => {
  assert.equal(t(f.rate(12.4e6, { unit: 'byte' })), '1.48 MiB/s');
  assert.equal(t(f.rate(0, { unit: 'byte' })), '0 B/s');
  assert.equal(t(f.rate(4000, { unit: 'byte' })), '<1 KiB/s');
  assert.equal(f.setRateUnit('byte'), 'byte');
  assert.equal(t(f.rate(9117730)), '1.09 MiB/s');
  f.setRateUnit('bit');
  assert.equal(t(f.rate(9117730)), '9.12 Mbps');
  assert.equal(f.getRateUnit(), 'bit');
});

test('§4.3 bandwidth and utilisation', () => {
  assert.equal(t(f.bandwidth(50e6)), '50 Mbps');
  assert.equal(t(f.bandwidth(1e9)), '1 Gbps');
  assert.equal(t(f.bandwidth(100e6)), '100 Mbps');
  assert.equal(t(f.bandwidth(2.5e9)), '2.5 Gbps');
  assert.equal(t(f.bandwidth(null)), '—');
  assert.equal(t(f.pctUtil(42.4)), '42%');
  assert.equal(t(f.pctUtil(0.003)), '<1%');
  assert.equal(t(f.pctUtil(0)), '0%');
  assert.equal(f.utilPair(42, 3).text, '↓42% ↑3%');
  assert.equal(f.utilPair(2.167, 2.043).text, '↓2% ↑2%'); // 线路机1
});

test('§4.4 percentages', () => {
  assert.equal(t(f.pctResource(45.3)), '45%');
  assert.equal(t(f.pctResource(7.2)), '7%');
  assert.equal(t(f.pctResource(45.34, { precise: true })), '45.3%');
  assert.equal(t(f.pctLoad(132)), '132%');
  assert.equal(t(f.loadLabel(132, { cores: 4 })), '负载 132%（5 分钟 · 4 核）');
  assert.equal(t(f.loadTriple(1.21, 1.32, 0.98)), '1.21 / 1.32 / 0.98');
  assert.equal(t(f.pctAvail(99.95)), '99.95%');
  assert.equal(t(f.pctAvail(87.3)), '87.30%');
  assert.equal(t(f.pctAvail(100)), '100%');
  assert.equal(t(f.pctAvail(99.999)), '99.99%'); // never rounds up to a perfect 100
  assert.equal(t(f.pctAvail(99.123)), '99.12%');
  assert.equal(t(f.pctLoss(0)), '0%');
  assert.equal(t(f.pctLoss(0.7)), '0.7%');
  assert.equal(t(f.pctLoss(1.51)), '1.5%');
  assert.equal(t(f.pctLoss(0.02)), '<0.1%');
  assert.equal(t(f.coverage(96.2)), '覆盖 96%');
  assert.equal(t(f.coverage(99.93)), '覆盖 99%');
  assert.equal(t(f.coverage(100)), '覆盖 100%');
  assert.equal(t(f.pctDelta(3.2)), '+3.2%');
  assert.equal(t(f.pctDelta(-1)), '−1.0%');
});

test('§5.4 micro-bar tone: ≥0.9×limit warn, ≥limit bad', () => {
  assert.equal(f.meterTone(50, 90), null);
  assert.equal(f.meterTone(81, 90), 'warn');
  assert.equal(f.meterTone(90, 90), 'bad');
  assert.equal(f.meterTone(null, 90), null);
});

test('§4.5 latency', () => {
  assert.equal(t(f.latency(42)), '42 ms');
  assert.equal(t(f.latency(1240)), '1.24 s');
  assert.equal(t(f.latency(0.4)), '<1 ms');
  assert.equal(t(f.latency(999.6)), '1.00 s');
  assert.equal(t(f.latency(null)), '—');
  assert.equal(t(f.latency(null, { failed: true })), '失败');
  assert.equal(t(f.latency(null, { timeout: true })), '超时');
  assert.equal(f.latency(491.6).exact, '491.6 ms');
  assert.equal(f.latencyTone(42, 'tcp'), 'ok');
  assert.equal(f.latencyTone(150, 'tcp'), 'warn');
  assert.equal(f.latencyTone(301, 'tcp'), 'bad');
  assert.equal(f.latencyTone(2900, 'egress', { latency_ms: 3000 }), 'ok');
  assert.equal(f.latencyTone(3100, 'egress', { latency_ms: 3000 }), 'warn');
  assert.equal(f.latencyTone('fail', 'egress'), 'bad');
  assert.equal(f.latencyTone(null, 'tcp'), null);
});

test('§4.6 money', () => {
  assert.equal(t(f.money(500)), '$5.00');
  assert.equal(t(f.money(123456)), '$1,234.56');
  assert.equal(t(f.money(128135)), '$1,281.35'); // kpis.monthly_cost
  assert.equal(t(f.money(500, { perMonth: true })), '$5.00/月');
  assert.equal(t(f.money(400, { currency: 'EUR' })), 'EUR 4.00');
  assert.equal(f.priceToCents('28.88'), 2888);
  assert.equal(f.priceToCents('59.00'), 5900);
  assert.equal(f.priceToCents('2.9'), 290);
  assert.equal(f.cycleKey('月付'), 'monthly');
  assert.equal(f.cycleWord('quarterly'), '季付');
  const q = f.monthlyFee({ price: '15.00', cycle: 'quarterly', currency: 'USD' });
  assert.equal(t(q), '$5.00');
  assert.equal(q.exact, '季付 $15.00');
  assert.equal(t(f.monthlyFee({ price_cents: 299, monthly_cents: 299, cycle: 'monthly' })), '$2.99');
  assert.equal(t(f.monthlyFee({ price: '0' })), '免费');
  assert.equal(t(f.monthlyFee({ pending: true })), '待定');
  assert.equal(t(f.monthlyFee(null)), '—');
  assert.equal(t(f.monthlyFee({ price: '99.00', cycle: 'lifetime' })), '长期');
  assert.deepEqual(f.moneyTotals([{ cents: 100 }, { cents: 250 }, { cents: 400, currency: 'EUR' }]).map(String), ['$3.50', 'EUR 4.00']);
});

test('§4.7 dates in Beijing time', () => {
  assert.equal(t(f.date(AS_OF)), '2026-09-23');
  assert.equal(t(f.stamp(AS_OF, { now: AS_OF })), '09-23 15:22');
  assert.equal(t(f.stamp(AS_OF, { now: AS_OF, seconds: true })), '09-23 15:22:18');
  assert.equal(t(f.stamp(1767196740, { now: AS_OF })), '2025-12-31 23:59');
  assert.equal(f.absolute(AS_OF), '2026-09-23 15:22:18 北京时间');
  assert.equal(f.clockTime(AS_OF), '15:22');
  // 16:30 UTC is already the next day in Beijing
  assert.equal(t(f.date(Date.UTC(2026, 8, 23, 16, 30) / 1000)), '2026-09-24');
});

test('§4.7 due countdown uses the billing zone calendar', () => {
  // 线路机1: due 2026-10-01 LA; at as_of it is 09-23 00:22 in LA → 8 days
  assert.equal(f.dueDays('2026-10-01', 'America/Los_Angeles', AS_OF), 8);
  assert.equal(f.dueDays('2026-10-22', 'Asia/Shanghai', AS_OF), 29);
  // 20:00 Beijing on 09-23 is still 09-23 05:00 in LA, but 09-24 00:30 Beijing is 09-23 09:30 LA
  const bjMidnight = Date.UTC(2026, 8, 23, 16, 30) / 1000;
  assert.equal(f.dueDays('2026-10-01', 'Asia/Shanghai', bjMidnight), 7);
  assert.equal(f.dueDays('2026-10-01', 'America/Los_Angeles', bjMidnight), 8);
  assert.equal(f.tzBadge('America/Los_Angeles'), 'LA');
  assert.equal(f.tzBadge('Asia/Shanghai'), null);
  const c = (d, o) => [t(f.countdown(d, o)), f.countdown(d, o).tone, f.countdown(d, o).tier];
  assert.deepEqual(c(-3), ['已过期 3 天', 'bad', 'expired']);
  assert.deepEqual(c(0), ['今天到期', 'bad', 'today']);
  assert.deepEqual(c(1), ['1 天', 'warn', 'soon']);
  assert.deepEqual(c(7), ['7 天', 'warn', 'soon']);
  assert.deepEqual(c(26), ['26 天', null, 'normal']);
  assert.deepEqual(c(36500), ['长期', 'secondary', 'long']);
  assert.deepEqual(c(12, { cycle: 'lifetime' }), ['长期', 'secondary', 'long']);
  assert.deepEqual(c(null), ['—', 'muted', 'unset']);
  assert.equal(f.isDue7(-3), true);
  assert.equal(f.isDue7(7), true);
  assert.equal(f.isDue7(8), false);
  const d = f.due({ due_date: '2026-10-01', billing_timezone: 'America/Los_Angeles', cycle: 'monthly' }, AS_OF);
  assert.equal(t(d), '8 天');
  assert.equal(d.badge, 'LA');
  assert.match(d.exact, /2026-10-01.*LA/);
  assert.equal(f.dueDate({ due_date: '2026-10-01', billing_timezone: 'America/Los_Angeles' }).badge, 'LA');
});

test('§4.8 relative time', () => {
  const ago = (s) => t(f.ago(AS_OF - s, { now: AS_OF }));
  assert.equal(ago(3), '刚刚');
  assert.equal(ago(38), '38 秒前');
  assert.equal(ago(125), '2 分钟前');
  assert.equal(ago(7300), '2 小时前');
  assert.equal(ago(3 * 86400), '3 天前');
  assert.equal(ago(8 * 86400), '09-15 15:22');
  assert.equal(t(f.ago(AS_OF + 600, { now: AS_OF })), '10 分钟后');
  assert.equal(t(f.ago(AS_OF + 7200, { now: AS_OF })), '2 小时后');
  assert.equal(f.ago(AS_OF - 38, { now: AS_OF }).exact, '2026-09-23 15:21:40 北京时间');
  assert.equal(f.refreshEvery(AS_OF - 30, AS_OF), 1000);
  assert.equal(f.refreshEvery(AS_OF - 90, AS_OF), 30000);
});

test('§4.8 injectable clock', () => {
  f.setClock(() => AS_OF);
  assert.equal(f.now(), AS_OF);
  assert.equal(t(f.ago(AS_OF - 38)), '38 秒前');
  assert.equal(f.dueDays('2026-10-01', 'America/Los_Angeles'), 8);
  f.setClock(() => Math.floor(Date.now() / 1000));
});

test('§4.8 durations and uptime', () => {
  assert.equal(t(f.duration(48)), '48 秒');
  assert.equal(t(f.duration(192)), '3 分 12 秒');
  assert.equal(t(f.duration(7500)), '2 小时 5 分');
  assert.equal(t(f.duration(3 * 86400 + 4 * 3600)), '3 天 4 小时');
  assert.equal(t(f.duration(180)), '3 分钟');
  assert.equal(t(f.duration(750, { units: 1 })), '12 分钟');
  assert.equal(t(f.duration(0)), '0 秒');
  assert.equal(t(f.uptime(12 * 86400 + 500)), '持续运行 12 天');
  assert.equal(t(f.uptime(22064)), '持续运行 6 小时 7 分');
});

test('§4.8 freshness', () => {
  assert.equal(f.freshness(4, 'komari').state, 'fresh');
  assert.equal(f.freshness(25, 'xray').state, 'delayed');
  assert.equal(f.freshness(25, 'xray').word, '延迟');
  assert.equal(f.freshness(51, 10).state, 'stale');
  assert.equal(f.freshness(51, 10).word, '数据过期');
  assert.equal(t(f.freshnessPill(2)), '实时 · 2 秒前');
  assert.equal(t(f.freshnessPill(25, 10)), '延迟 · 25 秒前');
  assert.equal(f.freshnessPill(25, 10).tone, 'warn');
  assert.equal(t(f.freshnessPill(60, 10)), '已断开');
  assert.equal(f.freshnessPill(60, 10).tone, 'bad');
});

test('§4.10 status words', () => {
  assert.deepEqual(['ok', 'degraded', 'fail', 'maintenance', 'unknown'].map((c) => f.health(c).word), ['正常', '降级', '故障', '维护中', '未知']);
  assert.equal(f.health('unknown').dot, 'ring');
  assert.equal(f.health('na').dot, 'none');
  assert.equal(f.health('ok', { stale: true }).dot, 'ring');
  assert.equal(f.health('ok', { stale: true }).modifier, '数据过期');
  assert.equal(f.health('故障').code, 'fail');
  assert.ok(f.health('fail').rank < f.health('degraded').rank);
  assert.equal(f.lifecycle('unbound').word, '尚未接入');
  assert.equal(f.step('rolled_back').word, '已回滚');
  assert.equal(f.step('skipped').word, '跳过');
  assert.equal(f.step('blocked').tone, 'bad');
  assert.equal(f.relayPart('xray', false).word, '已停止');
  assert.equal(f.relayPart('panel', true).word, '可达');
  assert.equal(f.relayPart('config', false).word, '待核对');
  assert.equal(f.entryWord('enable', false), '停用');
  assert.equal(f.entryWord('activity', true), '有活动');
  assert.equal(f.accessAction('blocked').word, '已阻止');
  assert.equal(f.incident({ open: true, severity: 'critical' }).word, '故障');
  assert.equal(f.incident({ open: true, severity: 'warning' }).word, '提醒');
  assert.equal(f.incident({ open: false, resolved_at: 1 }).word, '已恢复');
  assert.equal(f.apiKey('scope', 'full').word, '完整权限');
  assert.equal(f.apiKey('scope', 'full').tone, 'bad');
});

test('§3.4 status second line', () => {
  const deg = { code: 'degraded', reason: 'egress_mismatch', since: AS_OF - 750, checked_at: AS_OF };
  assert.equal(t(f.statusLine(deg, AS_OF)), '出口变更 · 12 分钟');
  assert.equal(t(f.statusLine({ code: 'ok', checked_at: AS_OF - 38 }, AS_OF)), '38 秒前');
  assert.equal(t(f.statusLine({ code: 'degraded', reason: 'retrying' }, AS_OF)), '重试中');
  assert.equal(t(f.statusLine({ code: 'fail', first_failed_check: 'ingress', since: AS_OF - 376 }, AS_OF)), '实际入口到出口 · 6 分钟');
});

test('§4.11 names and identifiers', () => {
  assert.equal(f.pad2(7), '07');
  assert.equal(f.shortName('团队1', 21), '团队1-21');
  assert.equal(f.entryLabel('线路机2', 20015), '线路机2:20015');
  assert.equal(f.entryLabel('线路机2', 20001, { master: true }), '线路机2 主节点');
  assert.equal(t(f.ip('198.51.100.41')), '198.51.100.41');
  const v6 = f.ip('2001:0db8:85a3:0000:0000:8a2e:0370:7334');
  assert.equal(t(v6), '2001:0db8…0370:7334');
  assert.equal(v6.exact, '2001:0db8:85a3:0000:0000:8a2e:0370:7334');
  assert.equal(t(f.shortId('5117f80b-1234-4abc-9def-001122334455')), '5117f80b…');
});

test('§4.1 grouping and number parts', () => {
  assert.equal(f.num(1234), '1,234');
  assert.equal(f.num(232721), '232,721');
  assert.equal(f.num(-1234.5, 1), '−1,234.5');
  assert.equal(t(f.int(140827)), '140,827');
  assert.equal(t(f.int(12, '个')), '12 个');
  assert.equal(f.round(1.005, 2), 1.01);
  assert.equal(f.html(f.bytes(1318123456)), '<span class="num">1.23</span> <span class="unit">GiB</span>');
  assert.equal(f.html(f.money(500)), '$<span class="num">5.00</span>');
  assert.equal(f.html('<b>'), '&lt;b&gt;');
  assert.equal(f.toneVar('warn'), 'var(--warn-text)');
  assert.equal(f.toneVar('ok', 'dot'), 'var(--ok-dot)');
  assert.equal(f.toneVar('muted'), 'var(--text-3)');
});

test('§4.12 glossary lint', () => {
  assert.deepEqual(f.lintTerms('访问已拦截，需要关注'), ['已拦截', '拦截', '需要关注']);
  assert.deepEqual(f.lintTerms('被拦截的连接'), ['拦截'], 'consistency #8: any 拦截 is flagged, not only 已拦截');
  assert.deepEqual(f.lintTerms('已阻止'), []);
  assert.ok(f.GLOSSARY.find((g) => g.term === '业务流量'));
});
