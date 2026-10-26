// 软路由的 mock：四台——团队2-路由A 在线（21 个出口：20 个对上 fleet 入口、其中 1 个参数不一致、1 个测试失败，外加 1 个路由自己配的；22 台客户端里 19 台在线）、
// 团队2-路由B 离线 3 小时、团队1-路由A 一键部署失败停在等待接入、团队1-路由B 在线且一切正常（8 个出口：5 个对上 fleet 入口、3 个客户自己加的）。两种接入都走一遍：一键部署几秒内走完几步变在线（SSH 地址 10.0.0.99 连接超时失败），
// 接入命令生成后 15 秒「路由执行了命令」变在线。每 15 秒「采集」一次，读数抖动、发 routers 事件。曲线按种子生成，刷新页面保持稳定。
import type { Incident, MachineView, SeriesPoint, Team } from '@/lib/types';
import { exitUsable, fleetExits, type RouterClient, type RouterDeploy, type RouterDetail, type RouterExit, type RouterGroup, type RouterSummary, type RouterView } from '@/lib/routers';
import { hashStr, rng } from './fixtures';

type Ctx = { params: string[]; query: URLSearchParams; body: any }; // eslint-disable-line @typescript-eslint/no-explicit-any
type Reg = (method: string, path: string, h: (c: Ctx) => unknown) => void;
interface Env {
  teams: () => Team[]; machines: () => MachineView[]; incidents: () => Incident[]; emit: (e: { type: string; id?: string }) => void;
  /** 在维护窗口里（自己的或全部的，同服务端） */
  maintenance: (id: string) => boolean;
  bad: (msg: string, status?: number, code?: string) => Error;
}
interface Rt { v: RouterView; d: RouterDetail | null; seed: number }

const MIN = 60_000; const HOUR = 60 * MIN; const DAY = 24 * HOUR; const GiB = 1024 ** 3;
const iso = (t = Date.now()) => new Date(t).toISOString();
const clone = <T,>(x: T): T => JSON.parse(JSON.stringify(x));
const RANGE: Record<string, [number, number]> = { '1h': [HOUR, 30], '6h': [6 * HOUR, 300], '24h': [DAY, 300], '7d': [7 * DAY, 3600], '30d': [30 * DAY, 3600] };
const DEPLOY_STEPS = ['连接 SSH', '检查环境（ssh、ucode、dropbear）', '写入隧道密钥与中控登录公钥', '安装并启动 fleet-tunnel', '等隧道连上中控'];
const HOSTS = ['iPhone-小王', 'MacBook-Pro', 'DESKTOP-8K2L1QF', 'iPad', 'Redmi-K60', 'HUAWEI-Mate60', 'LAPTOP-J3H9', 'MiTV-4A', null, 'android-7f3c2a', 'Galaxy-S23', 'DESKTOP-PC02', null, 'iPhone-15', 'NAS-DS220', 'XiaoAi-Speaker', 'ThinkPad-X1', 'Surface-Go', 'MacBook-Air', null, 'printer-hp', 'OPPO-Reno'];

export function installRoutersMock(R: Reg, env: Env) {
  const list: Rt[] = [];
  let seq = 5;
  const find = (id: string) => { const r = list.find((x) => x.v.id === id); if (!r) throw env.bad('软路由不存在', 404, 'not_found'); return r; };
  const team = (id: string | null | undefined) => { if (!id) return null; const t = env.teams().find((x) => x.id === id); if (!t) throw env.bad('团队不存在', 404, 'not_found'); return { id: t.id, name: t.name }; };
  const changed = () => env.emit({ type: 'routers' });

  /** 出口对应的 fleet 入口：在用节点的入口（团队2 先），server 是线路机的公网地址 */
  function fleetEntries() {
    const ms = env.machines();
    const relayHost = (id: string) => ms.find((m) => m.id === id)?.relay?.public_host ?? '203.0.113.10';
    return ms.filter((m) => m.kind !== 'relay' && m.lifecycle === 'active').sort((a, b) => (a.team?.id === 't2' ? 0 : 1) - (b.team?.id === 't2' ? 0 : 1))
      .flatMap((m) => m.entries.filter((e) => e.lifecycle === 'active').map((e) => ({ server: relayHost(e.relay.id), port: e.port, entry: { id: e.id, name: `${m.name} · ${e.relay.name}:${e.port}`, node_id: m.id, relay_id: e.relay.id, lifecycle: e.lifecycle } })));
  }
  /** dead：连续两次测不通（不可用）；flaky：只测不通 1 次（还算可用） */
  function buildDetail(seed: number, n: number, opts: { drift?: number; dead?: number; flaky?: number; other?: number; clients: number; lanBypass?: boolean }): RouterDetail {
    const r = rng(seed); const now = Date.now();
    const fleet = fleetEntries();
    const exits: RouterExit[] = Array.from({ length: n }, (_, i) => {
      const f = i < n - (opts.other ?? 0) ? fleet[i] : undefined; // 末尾 other 个是客户自己加的出口（不对应 fleet 入口）
      const dead = i + 1 === opts.dead; const flaky = i + 1 === opts.flaky;
      return {
        name: `节点${String(i + 1).padStart(2, '0')}`, type: 'Vless', server: f?.server ?? `198.51.100.${30 + i}`, port: f?.port ?? 443, entry: f?.entry ?? null,
        sync: !f ? 'unmanaged' : i + 1 === opts.drift ? 'drift' : 'ok', alive: !dead && !flaky, delay_ms: dead || flaky ? null : Math.round(60 + r() * 140), delay_at: iso(now - Math.floor(r() * 5) * MIN),
        fail_streak: dead ? 3 : flaky ? 1 : 0,
        conns: dead ? 0 : Math.floor(r() * 60), up_bps: dead ? 0 : r() * 3e6, down_bps: dead ? 0 : r() * 2.4e7, clients: dead ? 0 : Math.floor(r() * 6),
      };
    });
    exits.push({ name: '节点00-默认', type: 'Vless', server: '198.51.100.7', port: 443, entry: null, sync: 'unmanaged', alive: true, delay_ms: 210, delay_at: iso(now - 2 * MIN), fail_streak: 0, conns: 3, up_bps: 4e4, down_bps: 2e5, clients: 1 });
    const clients: RouterClient[] = Array.from({ length: opts.clients }, (_, i) => {
      const online = i < opts.clients - 3;
      const via = exits.filter((e) => e.alive && e.conns > 0).sort(() => r() - 0.5).slice(0, online ? 1 + Math.floor(r() * 3) : 0);
      const ex = via.map((e) => ({ name: e.name, conns: 1 + Math.floor(r() * 20) }));
      return {
        ip: `192.168.124.${101 + i}`, mac: opts.lanBypass ? null : Array.from({ length: 6 }, () => Math.floor(r() * 256).toString(16).padStart(2, '0')).join(':'),
        hostname: opts.lanBypass ? null : HOSTS[i % HOSTS.length], online, conns: ex.reduce((s, x) => s + x.conns, 0), down_bps: online ? Math.round(r() * 1.2e7) : 0, up_bps: online ? Math.round(r() * 9e5) : 0, exits: ex,
      };
    });
    // 策略组：两个 select 组（可临时切换）和一个 url-test 组（自动选择，不能手动切）
    const names = exits.map((e) => e.name);
    // GLOBAL 是 mihomo 自带的：规则不用它（生产上的软路由多半只有它），所以 used = false，不给切换
    const groups: RouterGroup[] = [
      { name: '节点选择', type: 'Selector', now: names[0], all: names, used: true },
      { name: '漏网之鱼', type: 'Selector', now: names[names.length - 1], all: ['REJECT', 'DIRECT', ...names.slice(-4)], used: true },
      { name: '自动选择', type: 'URLTest', now: names[1] ?? names[0], all: names, used: true },
      { name: 'GLOBAL', type: 'Selector', now: 'DIRECT', all: ['DIRECT', ...names], used: false },
    ];
    return {
      system: { os: 'iStoreOS 24.10.2', kernel: '6.6.73', cpu_model: r() > 0.5 ? 'Intel(R) Celeron(R) CPU J1900 @ 1.99GHz' : 'Intel(R) Core(TM) i3-5005U CPU @ 2.00GHz', cores: 4, load: [0.49, 0.62, 0.58], procs: 168 + Math.floor(r() * 40), clock_skew_s: +(r() * 2 - 1).toFixed(1), clash_rss: (90 + r() * 80) * 1024 * 1024 },
      interfaces: opts.lanBypass
        ? [{ name: 'wan', device: 'br-lan', up: true, proto: 'dhcp', ip: '192.168.31.81', uptime_s: 16862, rx_bps: 2.1e7, tx_bps: 3.2e6, rx_bytes: 412 * GiB, tx_bytes: 61 * GiB }]
        : [
          { name: 'wan', device: 'eth0', up: true, proto: 'dhcp', ip: '192.168.1.2', uptime_s: 16862, rx_bps: 3.2e7, tx_bps: 4.1e6, rx_bytes: 812 * GiB, tx_bytes: 96 * GiB },
          { name: 'wan6', device: 'eth0', up: false, proto: 'dhcpv6', ip: null, uptime_s: null, rx_bps: 0, tx_bps: 0, rx_bytes: 0, tx_bytes: 0 },
          { name: 'lan', device: 'br-lan', up: true, proto: 'static', ip: '192.168.124.1', uptime_s: 16901, rx_bps: 3.9e6, tx_bps: 2.9e7, rx_bytes: 88 * GiB, tx_bytes: 790 * GiB },
        ],
      openclash: { enabled: true, running: true, version: 'alpha-g99e68e9', mode: 'fake-ip-mix', config_path: opts.lanBypass ? '/etc/openclash/config/customer-2nodes.yaml' : '/etc/openclash/config/clash-team2-20nodes.yaml', config_mtime: iso(now - 3 * DAY) },
      exits, clients, collected_at: iso(now), groups,
    };
  }
  const exitEntries = (d: RouterDetail) => d.exits.filter((e) => e.entry).map((e) => ({ exit: e.name, entry_id: e.entry!.id }));
  /** 摘要由详情算（每轮采集的读数先在详情上抖动） */
  function summarize(d: RouterDetail, seed: number, uptime: number): RouterSummary {
    const r = rng(seed ^ Math.floor(Date.now() / 15_000));
    const wan = d.interfaces[0];
    const memTotal = 3.8 * GiB;
    return {
      uptime_s: uptime, cpu_pct: +(4 + r() * 14).toFixed(1), load1: d.system.load[0], cores: d.system.cores, mem_used: memTotal * (0.32 + r() * 0.08), mem_total: memTotal,
      disk_used: 0.41 * 1.9 * GiB, disk_total: 1.9 * GiB, conntrack: 1500 + Math.floor(r() * 900), conntrack_max: 65535, temp_c: 38 + Math.round(r() * 8),
      wan: { up: wan.up, ip: wan.ip, rx_bps: wan.rx_bps, tx_bps: wan.tx_bps },
      openclash: { enabled: d.openclash.enabled, running: d.openclash.running, version: d.openclash.version, mode: d.openclash.mode, conns: d.exits.reduce((s, e) => s + e.conns, 0), up_bps: d.exits.reduce((s, e) => s + e.up_bps, 0), down_bps: d.exits.reduce((s, e) => s + e.down_bps, 0) },
      exits: {
        total: d.exits.length, alive: d.exits.filter(exitUsable).length, dead: d.exits.filter((e) => !exitUsable(e)).map((e) => e.name), managed: d.exits.filter((e) => e.entry).length, drift: d.exits.filter((e) => e.sync === 'drift').length,
        fleet: { total: d.exits.filter((e) => e.sync !== 'unmanaged').length, alive: d.exits.filter((e) => e.sync !== 'unmanaged' && exitUsable(e)).length, dead: d.exits.filter((e) => e.sync !== 'unmanaged' && !exitUsable(e)).map((e) => e.name) },
        other: d.exits.filter((e) => e.sync === 'unmanaged').length,
      },
      clients: d.clients.filter((c) => c.online).length,
    };
  }
  const base = (id: string, name: string, teamId: string | null, port: number, createdAgo: number): RouterView => ({
    id, name, team: team(teamId), note: '', tunnel_port: port, created_at: iso(Date.now() - createdAgo), state: 'pending', last_seen_at: null, error: null, summary: null, deploy: null, maintenance: false, exit_entries: [],
  });
  /** 变在线：第一次采集（新接入的 9 个出口、旁路由，像客户现场的 team2） */
  function goOnline(rt: Rt) {
    rt.d ??= buildDetail(rt.seed, 9, { clients: 6, lanBypass: true });
    rt.v.state = 'online'; rt.v.error = null; rt.v.last_seen_at = iso(); rt.v.summary = summarize(rt.d, rt.seed, 120); rt.v.exit_entries = exitEntries(rt.d);
  }
  let seeded = false;
  function seed() {
    if (seeded) return; seeded = true;
    const now = Date.now();
    const a: Rt = { v: base('rt-a', '团队2-路由A', 't2', 23001, 20 * DAY), d: null, seed: hashStr('rt-a') };
    a.v.note = '客户机房 2 楼弱电箱，主路由（发 DHCP）';
    a.d = buildDetail(a.seed, 20, { drift: 7, dead: 12, flaky: 15, clients: 22 });
    a.v.state = 'online'; a.v.last_seen_at = iso(now - 8000); a.v.summary = summarize(a.d, a.seed, 16862); a.v.exit_entries = exitEntries(a.d);
    a.v.deploy = { state: 'done', step: null, error: null, started_at: iso(now - 20 * DAY), finished_at: iso(now - 20 * DAY + 40_000) };
    // 进行中的临时切换：「节点选择」从 节点01 临时切到 节点03，3 小时后自动切回
    const sel = a.d.groups![0]; const orig = sel.now!; sel.now = a.d.exits[2].name;
    a.v.switches = [{ group: sel.name, from: orig, to: sel.now, back_at: iso(now + 3 * HOUR), by: 'admin', switched_at: iso(now - 25 * MIN), defer_why: null, deferred_at: null }];
    // 另一条已到时间、但原出口还不通：往后推了 1 小时，等原出口恢复后再切回
    const sel2 = a.d.groups![1]; const orig2 = sel2.now!; sel2.now = 'DIRECT';
    a.v.switches.push({ group: sel2.name, from: orig2, to: 'DIRECT', back_at: iso(now + 40 * MIN), by: '系统', switched_at: iso(now - 5 * HOUR), defer_why: `原出口「${orig2}」现在不通（连续 3 次测延迟不通）`, deferred_at: iso(now - 20 * MIN) });
    const b: Rt = { v: base('rt-b', '团队2-路由B', 't2', 23002, 18 * DAY), d: null, seed: hashStr('rt-b') };
    b.d = buildDetail(b.seed, 20, { clients: 19 });
    b.v.state = 'offline'; b.v.last_seen_at = iso(now - 3 * HOUR - 12 * MIN); b.v.summary = summarize(b.d, b.seed, 912_000); b.v.exit_entries = exitEntries(b.d);
    b.v.error = 'ssh: connect to host 127.0.0.1 port 23002: Connection refused（隧道没连上：路由断电、断网，或 fleet-tunnel 没在运行）';
    const c: Rt = { v: base('rt-c', '团队1-路由A', 't1', 23003, 6 * MIN), d: null, seed: hashStr('rt-c') };
    c.v.note = '旁路由';
    c.v.deploy = { state: 'failed', step: '连接 SSH', error: 'ssh: connect to host 10.0.0.99 port 22: Connection timed out', started_at: iso(now - 5 * MIN), finished_at: iso(now - 5 * MIN + 30_000) };
    // 在线、没有参数不一致、也没有出口不通；另有 3 个客户自己加的出口（不对应面板入口，不计入卡片的出口数）
    const d: Rt = { v: base('rt-d', '团队1-路由B', 't1', 23004, 9 * DAY), d: null, seed: hashStr('rt-d') };
    d.v.note = '门店收银旁，旁路由';
    d.d = buildDetail(d.seed, 8, { other: 3, clients: 7, lanBypass: true });
    d.v.state = 'online'; d.v.last_seen_at = iso(now - 5000); d.v.summary = summarize(d.d, d.seed, 410_000); d.v.exit_entries = exitEntries(d.d);
    d.v.deploy = { state: 'done', step: null, error: null, started_at: iso(now - 9 * DAY), finished_at: iso(now - 9 * DAY + 38_000) };
    list.push(a, b, c, d);
    const inc = (id: string, rt: Rt, rule: string, rule_name: string, severity: string, summary: string, ago: number): Incident => ({
      id, engine: 'router', rule, rule_name, subject: { id: rt.v.id, name: rt.v.name, kind: 'router' }, subject_id: rt.v.id, subject_name: rt.v.name, severity, state: 'firing', summary,
      opened_at: iso(now - ago), last_seen_at: iso(now - MIN), resolved_at: null, ack_at: null, ack_by: null, reopen_count: 0,
    });
    env.incidents().push(
      inc('i-rt1', b, 'router_offline', '软路由离线', 'critical', '团队2-路由B：连不上路由已 3 小时（隧道没连上）', 3 * HOUR),
      inc('i-rt2', a, 'router_exits_down', '软路由出口不通', 'warning', '团队2-路由A：1 个出口连续两次延迟测试失败：节点12', 40 * MIN),
    );
  }
  seed();

  /** 一键部署：每 1.5 秒走一步，走完变在线；地址是 10.0.0.99 的第一步连接超时 */
  function startDeploy(rt: Rt, host: string, port: number) {
    const d: RouterDeploy = { state: 'running', step: DEPLOY_STEPS[0], error: null, started_at: iso(), finished_at: null };
    rt.v.deploy = d; changed();
    let i = 0;
    const tick = () => {
      if (rt.v.deploy !== d || !list.includes(rt)) return;
      if (host === '10.0.0.99') { Object.assign(d, { state: 'failed', error: `ssh: connect to host ${host} port ${port}: Connection timed out`, finished_at: iso() }); changed(); return; }
      if (++i < DEPLOY_STEPS.length) { d.step = DEPLOY_STEPS[i]; changed(); setTimeout(tick, 1500); return; }
      Object.assign(d, { state: 'done', step: null, finished_at: iso() }); goOnline(rt); changed();
    };
    setTimeout(tick, host === '10.0.0.99' ? 2500 : 1500);
  }
  /** 接入命令：生成后 15 秒「路由执行了命令」，变在线 */
  function bootstrap(rt: Rt) {
    const token = Math.random().toString(36).slice(2, 14) + Math.random().toString(36).slice(2, 14);
    setTimeout(() => { if (list.includes(rt)) { goOnline(rt); changed(); } }, 15_000);
    return { cmd: `curl -fsS https://fleet.example.com/api/v1/router-bootstrap/${token} | sh`, expires_at: iso(Date.now() + 30 * MIN) };
  }

  const view = (rt: Rt) => ({ ...clone(rt.v), maintenance: env.maintenance(rt.v.id) });
  R('GET', '/routers', () => list.map(view));
  R('GET', '/routers/([^/]+)', ({ params }) => { const rt = find(params[0]); return { ...view(rt), detail: rt.v.summary ? clone(rt.d) : null }; });
  R('POST', '/routers', ({ body }) => {
    const name = String(body?.name ?? '').trim();
    if (!name) throw env.bad('请填写名称');
    if (list.some((x) => x.v.name === name)) throw env.bad('已有同名的软路由', 409, 'conflict');
    const id = `rt-${seq}`;
    const rt: Rt = { v: { ...base(id, name, body.team_id, 23000 + seq++, 0), note: body.note ?? '' }, d: null, seed: hashStr(id) };
    list.push(rt);
    if (body.ssh) { startDeploy(rt, body.ssh.host, body.ssh.port); return { id }; }
    changed();
    return { id, bootstrap: bootstrap(rt) };
  });
  R('POST', '/routers/([^/]+)/deploy', ({ params, body }) => {
    const rt = find(params[0]);
    if (rt.v.deploy?.state === 'running') throw env.bad('这台路由正在部署，等这次结束再试', 409, 'conflict');
    startDeploy(rt, body.host, body.port);
    return { ok: true };
  });
  // 重新生成：换隧道密钥，连上过的旧隧道立刻断
  R('POST', '/routers/([^/]+)/bootstrap', ({ params }) => {
    const rt = find(params[0]);
    if (rt.v.state === 'online') { rt.v.state = 'offline'; rt.v.error = '隧道密钥已更换：等路由执行新的接入命令'; }
    const b = bootstrap(rt); changed(); return b;
  });
  R('PATCH', '/routers/([^/]+)', ({ params, body }) => {
    const rt = find(params[0]);
    if (body.name != null) {
      const n = String(body.name).trim();
      if (!n) throw env.bad('请填写名称');
      if (list.some((x) => x !== rt && x.v.name === n)) throw env.bad('已有同名的软路由', 409, 'conflict');
      rt.v.name = n;
    }
    if ('team_id' in body) rt.v.team = team(body.team_id);
    if (body.note != null) rt.v.note = String(body.note);
    changed(); return clone(rt.v);
  });
  R('DELETE', '/routers/([^/]+)', ({ params }) => { const rt = find(params[0]); list.splice(list.indexOf(rt), 1); changed(); return {}; });

  // 临时切换（select 组）：改组当前选择，记原来是谁；切回到原来的
  const selGroup = (rt: Rt, name: string) => {
    const g = rt.d?.groups?.find((x) => x.name === name && x.type === 'Selector');
    if (!rt.d || rt.v.state !== 'online') throw env.bad('路由不在线，切不了', 409, 'conflict');
    if (!g) throw env.bad('没有这个可切换的策略组', 404, 'not_found');
    return g;
  };
  R('POST', '/routers/([^/]+)/switch', ({ params, body }) => {
    const rt = find(params[0]); const g = selGroup(rt, String(body.group));
    if (g.used === false) throw env.bad(`没有规则用策略组「${g.name}」，切了不影响流量；要把客户挪走用改出口指向（fleetctl repoint）`, 409, 'conflict');
    if (!g.all.includes(body.to) || body.to === g.now) throw env.bad('要切到的出口不在组里，或就是现在的');
    const prev = rt.v.switches?.find((s) => s.group === g.name); const from = g.now!; const original = prev?.from ?? from;
    rt.v.switches = (rt.v.switches ?? []).filter((s) => s.group !== g.name);
    g.now = body.to;
    const ended = g.now === original; const back_at = body.back_after_s && !ended ? iso(Date.now() + body.back_after_s * 1000) : null;
    if (!ended) rt.v.switches.push({ group: g.name, from: original, to: g.now!, back_at, by: 'admin', switched_at: iso() });
    changed(); return { from, to: g.now, original, back_at, ended };
  });
  R('POST', '/routers/([^/]+)/switch/back', ({ params, body }) => {
    const rt = find(params[0]); const g = selGroup(rt, String(body.group));
    const s = rt.v.switches?.find((x) => x.group === g.name);
    if (!s) throw env.bad('这个组没有进行中的临时切换', 404, 'not_found');
    const from = g.now!; g.now = s.from; rt.v.switches = rt.v.switches!.filter((x) => x !== s);
    changed(); return { from, to: g.now };
  });

  // 配置变更：在线的路由有几条固定的（一条作业改的、一条路由上改的、一条只改了出口以外的部分），别的没有
  R('GET', '/routers/([^/]+)/changes', ({ params, query }) => {
    const rt = find(params[0]);
    const ex = rt.d?.exits ?? [];
    const at = (ago: number) => iso(Date.now() - ago);
    const all = rt.v.summary && ex.length > 1 ? [
      { id: `${rt.v.id}-c3`, at: at(20 * MIN), old_sha: 'c1d2e3f4a5b6', new_sha: '9f8e7d6c5b4a', source: 'router', job_id: null, job_kind: null, diff: [], note: null },
      { id: `${rt.v.id}-c2`, at: at(3 * HOUR), old_sha: '7a6b5c4d3e2f', new_sha: 'c1d2e3f4a5b6', source: 'job', job_id: 'mock-job', job_kind: 'router_sync', diff: [{ exit: ex[0].name, op: 'changed', creds: true }], note: null },
      { id: `${rt.v.id}-c1`, at: at(2 * DAY), old_sha: '0a1b2c3d4e5f', new_sha: '7a6b5c4d3e2f', source: 'router', job_id: null, job_kind: null,
        diff: [{ exit: ex[1].name, op: 'changed', from: `${ex[1].server}:${ex[1].port}`, to: `${ex[1].server}:${(ex[1].port ?? 20000) + 1}`, creds: true }, { exit: '新出口', op: 'added', to: '198.51.100.7:20031' }, { exit: '旧出口', op: 'removed', from: '198.51.100.8:20012' }], note: null },
    ] : [];
    return all.slice(0, Number(query.get('limit') ?? 200));
  });

  // 曲线：按种子的平滑起伏（白天高、夜里低），离线的停在最后在线的时刻，没接入的没有点
  R('GET', '/routers/([^/]+)/metrics', ({ params, query }) => {
    const rt = find(params[0]);
    const [span, step] = RANGE[query.get('range') ?? '6h'] ?? RANGE['6h'];
    const s = rt.v.summary;
    if (!s || !rt.v.last_seen_at) return { step_s: step, points: [] };
    const end = Date.parse(rt.v.last_seen_at); const start = Math.max(end - span, Date.parse(rt.v.created_at));
    const r = rng(rt.seed); const fx = fleetExits(s);
    const points: SeriesPoint[] = [];
    for (let t = start - (start % (step * 1000)) + step * 1000; t <= end; t += step * 1000) {
      const day = 0.55 + 0.45 * Math.sin(((t / HOUR) % 24) / 24 * Math.PI * 2 - 1.6); const n = () => 0.8 + r() * 0.4;
      points.push({
        ts: iso(t), cpu_pct: +(s.cpu_pct * day * n()).toFixed(1), load1: +(s.load1 * day * n()).toFixed(2), mem_pct: +((s.mem_used / s.mem_total) * 100 * (0.95 + r() * 0.1)).toFixed(1),
        disk_pct: +((s.disk_used / s.disk_total) * 100).toFixed(1), conntrack_pct: +((s.conntrack / s.conntrack_max) * 100 * day * n()).toFixed(2), temp_c: s.temp_c == null ? null : +(s.temp_c - 3 + day * 5 * n()).toFixed(1),
        wan_rx_bps: Math.round(s.wan.rx_bps * day * n()), wan_tx_bps: Math.round(s.wan.tx_bps * day * n()), proxy_up_bps: Math.round(s.openclash.up_bps * day * n()), proxy_down_bps: Math.round(s.openclash.down_bps * day * n()),
        conns: Math.round(s.openclash.conns * day * n()), clients: Math.max(1, Math.round(s.clients * (0.6 + day * 0.4))), exits_alive: t > end - 40 * MIN ? s.exits.alive : s.exits.total,
        fleet_alive: t > end - 40 * MIN ? fx.alive : fx.total,
      });
    }
    return { step_s: step, points };
  });
  // 各出口：测试失败的出口近 40 分钟全失败、之前偶有失败；其余偶尔一次失败
  R('GET', '/routers/([^/]+)/exits/metrics', ({ params, query }) => {
    const rt = find(params[0]);
    const [span, step] = RANGE[query.get('range') ?? '6h'] ?? RANGE['6h'];
    if (!rt.d || !rt.v.summary || !rt.v.last_seen_at) return { step_s: step, exits: [] };
    const end = Date.parse(rt.v.last_seen_at); const start = Math.max(end - span, Date.parse(rt.v.created_at));
    return {
      step_s: step,
      exits: rt.d.exits.map((e) => {
        const r = rng(rt.seed ^ hashStr(e.name)); const base = e.delay_ms ?? 120;
        const points: SeriesPoint[] = [];
        for (let t = start - (start % (step * 1000)) + step * 1000; t <= end; t += step * 1000) {
          const fail = e.alive === false ? t > end - 40 * MIN || r() < 0.15 : r() < 0.02;
          points.push({ ts: iso(t), delay_ms: fail ? null : Math.round(base * (0.8 + r() * 0.5)), alive: !fail, conns: fail ? 0 : Math.round(e.conns * (0.5 + r())), up_bps: fail ? 0 : Math.round(e.up_bps * (0.5 + r())), down_bps: fail ? 0 : Math.round(e.down_bps * (0.5 + r())) });
        }
        return { name: e.name, points };
      }),
    };
  });

  // 采集：在线的每 15 秒读数抖一抖（速率、连接、延迟），发一次 routers 事件
  setInterval(() => {
    let any = false;
    for (const rt of list) {
      if (rt.v.state !== 'online' || !rt.d || rt.v.deploy?.state === 'running') continue;
      const r = rng(rt.seed ^ Math.floor(Date.now() / 1000));
      for (const i of rt.d.interfaces) if (i.up) { i.rx_bps = Math.max(1e5, i.rx_bps * (0.7 + r() * 0.6)); i.tx_bps = Math.max(5e4, i.tx_bps * (0.7 + r() * 0.6)); }
      // 结构共享：只换读数变了的出口，没变的引用不变（抽屉表格的行 memo 靠它）
      rt.d.exits = rt.d.exits.map((e) => (e.alive && r() < 0.4 ? { ...e, conns: Math.max(0, e.conns + Math.round(r() * 10 - 5)), down_bps: e.down_bps * (0.7 + r() * 0.6), up_bps: e.up_bps * (0.7 + r() * 0.6) } : e));
      rt.d.collected_at = iso();
      const up = rt.v.summary!.uptime_s + 15;
      rt.v.summary = summarize(rt.d, rt.seed, up); rt.v.exit_entries = exitEntries(rt.d); rt.v.last_seen_at = iso();
      any = true;
    }
    if (any) changed();
  }, 15_000);
  return { routers: () => list.map((x) => x.v) };
}
