// fleet 软路由采集：中控每 30 秒经隧道执行一次（随 fleet-server 编进去，经 stdin 交给 sh -s，路由上不装东西），只读。
// 输出一行紧凑 JSON：系统、接口、OpenClash / mihomo 的汇总（每个出口、每个来源 IP 的连接数与这段时间的流量）、DHCP 租约、邻居表。
// FLEET_MODE=delay 时只测出口延迟（每 5 分钟一次，中控另起一次执行、单独的时限），输出 {v, now, oc: {delay}}；
// FLEET_MANAGED 是 fleet 出口（对上了 fleet 入口的）的名字（JSON 数组）：先测它们，没通的重测名额也先给它们。
// mihomo 的 API 密钥（uci 的 dashboard_password）只在本机读来用，写在 0600 的 /tmp/fleet-collect/auth 里给 curl，不回传。
// 上一次的 CPU 计数、各连接的字节数存在 /tmp/fleet-collect/（tmpfs），用来算 CPU 使用率与各出口这段时间的流量
// （同一连接两次之差累加，新连接全算，关掉的连接最后一段会漏掉：估计值）。
'use strict';

let fs = require('fs');

const D = '/tmp/fleet-collect';
const MODE = getenv('FLEET_MODE');
const DELAY_URL = getenv('FLEET_DELAY_URL');
let MANAGED = {};
try {
	for (let x in (json(getenv('FLEET_MANAGED') || '[]') || []))
		MANAGED[x] = true;
}
catch (e) {}
// mihomo /proxies 里不是出口的：内置的与各种组（PassRule 是较新 alpha 内核才有的内置 PASS-RULE）
const SKIP = { Direct: true, Reject: true, RejectDrop: true, Pass: true, PassRule: true, Compatible: true, Dns: true,
	Selector: true, URLTest: true, Fallback: true, LoadBalance: true, Relay: true, Smart: true };

let out = { v: 1, now: time() };

function rd(p) {
	let s = fs.readfile(p);
	return (s == null) ? null : trim(s);
}

function run(cmd) {
	let h = fs.popen(cmd, 'r');
	if (h == null)
		return null;
	let s = h.read('all');
	h.close();
	return s;
}

function num(s) {
	if (s == null || s === '')
		return null;
	let x = +s;
	return (x == x) ? x : null;
}

function q(s) {
	return "'" + replace(s, /'/g, "'\\''") + "'";
}

function lines(s) {
	return (s == null) ? [] : split(s, '\n');
}

function words(s) {
	let t = trim(s || '');
	return (t == '') ? [] : split(t, /[ \t]+/);
}

function system() {
	// CPU：/proc/stat 第一行的 user nice system idle iowait irq softirq steal（guest 已算在 user 里），和上一次的差
	let f = words(split(fs.readfile('/proc/stat') || '', '\n')[0]);
	let busy = 0, total = 0;
	for (let i = 1; i <= 8 && i < length(f); i++) {
		let v = int(f[i]);
		total += v;
		if (i != 4 && i != 5)
			busy += v;
	}
	let prev = words(rd(D + '/cpu.prev'));
	if (length(prev) == 2 && total > int(prev[1]))
		out.cpu_pct = (busy - int(prev[0])) * 100.0 / (total - int(prev[1]));
	fs.writefile(D + '/cpu.prev', sprintf('%d %d', busy, total));

	let la = words(rd('/proc/loadavg'));
	out.load = [num(la[0]), num(la[1]), num(la[2])];
	out.uptime = num(words(rd('/proc/uptime'))[0]);

	let procs = 0;
	for (let e in (fs.lsdir('/proc') || []))
		if (match(e, /^[0-9]+$/))
			procs++;
	out.procs = procs;

	let cores = 0;
	for (let l in lines(fs.readfile('/proc/cpuinfo'))) {
		if (match(l, /^processor[ \t]*:/))
			cores++;
		let m = match(l, /^(model name|cpu model|Hardware)[ \t]*:[ \t]*(.+)$/);
		if (m && out.cpu_model == null)
			out.cpu_model = trim(m[2]);
	}
	out.cores = cores;

	for (let l in lines(fs.readfile('/proc/meminfo'))) {
		let m = match(l, /^(MemTotal|MemAvailable):[ \t]+([0-9]+)/);
		if (m)
			out[(m[1] == 'MemTotal') ? 'mem_total' : 'mem_avail'] = int(m[2]) * 1024;
	}

	// 存储：有 /overlay 看它（可写层），否则看 /；路径报 df 给的挂载点（没单独挂 /overlay 时就是 /）。
	// 总量 = 已用 + 可用（同 df 的 Use%）。文件系统名太长时 busybox df 会折成两行，数字在最后一行
	let dp = fs.stat('/overlay') ? '/overlay' : '/';
	let df = filter(lines(run('df -k ' + dp + ' 2>/dev/null')), (l) => trim(l) != '');
	let f2 = words(df[length(df) - 1] || '');
	let n = length(f2);
	if (n >= 5 && num(f2[n - 4]) != null)
		out.disk = { path: f2[n - 1], used: int(f2[n - 4]) * 1024, avail: int(f2[n - 3]) * 1024 };

	// 温度：thermal_zone 与 hwmon 里最高的一个（0–150 ℃ 之外的读数不要）
	let temp = null;
	let consider = (p) => {
		let t = num(rd(p));
		if (t != null) {
			t = t / 1000.0;
			if (t > 0 && t < 150 && (temp == null || t > temp))
				temp = t;
		}
	};
	for (let z in (fs.lsdir('/sys/class/thermal') || []))
		if (match(z, /^thermal_zone/))
			consider('/sys/class/thermal/' + z + '/temp');
	for (let h in (fs.lsdir('/sys/class/hwmon') || []))
		for (let x in (fs.lsdir('/sys/class/hwmon/' + h) || []))
			if (match(x, /^temp[0-9]+_input$/))
				consider('/sys/class/hwmon/' + h + '/' + x);
	out.temp_c = temp;

	out.conntrack = num(rd('/proc/sys/net/netfilter/nf_conntrack_count'));
	out.conntrack_max = num(rd('/proc/sys/net/netfilter/nf_conntrack_max'));
	for (let l in lines(fs.readfile('/etc/openwrt_release'))) {
		let m = match(l, /^DISTRIB_DESCRIPTION=['"]?([^'"]*)/);
		if (m)
			out.os = trim(m[1]);
	}
	out.kernel = rd('/proc/sys/kernel/osrelease');
}

function ifaces() {
	let d = null;
	try {
		d = json(run('ubus call network.interface dump 2>/dev/null') || '{}');
	}
	catch (e) {
		d = {};
	}
	out.ifaces = [];
	let devs = {};
	for (let i in (d.interface || [])) {
		let a = i['ipv4-address'];
		let dev = i.l3_device || i.device;
		push(out.ifaces, {
			name: i.interface, up: i.up ? true : false, proto: i.proto, device: dev,
			ip: (type(a) == 'array' && length(a) > 0) ? a[0].address : null, uptime: i.uptime
		});
		if (dev)
			devs[dev] = true;
	}
	out.dev = {};
	for (let l in lines(fs.readfile('/proc/net/dev'))) {
		let m = match(l, /^[ \t]*([^: \t]+):(.*)$/);
		if (!m || !devs[m[1]])
			continue;
		let f = words(m[2]);
		out.dev[m[1]] = [num(f[0]), num(f[8])];
	}
}

function uci(k) {
	let s = trim(run('uci -q get openclash.config.' + k + ' 2>/dev/null') || '');
	return (s == '') ? null : s;
}

// 调 mihomo 的 API：{code, data} 或 {code, error}
function api(port, path, tmo) {
	let s = run(sprintf("curl -sS -m %d -H @%s -w '\\n%%{http_code}' %s 2>&1", tmo, D + '/auth', q('http://127.0.0.1:' + port + path)));
	if (s == null)
		return { code: 0, error: 'curl 运行失败' };
	let i = rindex(s, '\n');
	let code = int(trim(substr(s, i + 1)));
	let body = (i > 0) ? substr(s, 0, i) : '';
	if (code != 200)
		return { code: code, error: substr(trim(body), 0, 200) };
	try {
		return { code: 200, data: json(body) };
	}
	catch (e) {
		return { code: 200, error: 'API 返回的不是 JSON' };
	}
}

function connections(oc, port) {
	let r = api(port, '/connections', 8);
	if (r.data == null) {
		oc.api_error = r.error;
		return;
	}
	let cj = r.data;
	oc.up_total = cj.uploadTotal;
	oc.down_total = cj.downloadTotal;
	let conns = cj.connections || [];
	oc.conns = length(conns);
	let prev = null, pts = null;
	try {
		let p = json(fs.readfile(D + '/conns.prev') || 'null');
		if (p) {
			prev = p.c;
			pts = p.t;
		}
	}
	catch (e) {
		prev = null;
	}
	let cur = {}, ex = {}, cl = {};
	for (let c in conns) {
		let name = (type(c.chains) == 'array' && length(c.chains) > 0) ? c.chains[0] : 'DIRECT';
		let ip = c.metadata ? c.metadata.sourceIP : null;
		let up = c.upload || 0, down = c.download || 0;
		let p = prev ? prev[c.id] : null;
		let du = p ? up - p[0] : up, dd = p ? down - p[1] : down;
		if (du < 0)
			du = up;
		if (dd < 0)
			dd = down;
		cur[c.id] = [up, down];
		if (!ex[name])
			ex[name] = { conns: 0, up: 0, down: 0, ips: {} };
		let e = ex[name];
		e.conns = e.conns + 1;
		e.up = e.up + du;
		e.down = e.down + dd;
		if (ip) {
			e.ips[ip] = true;
			if (!cl[ip])
				cl[ip] = { conns: 0, exits: {}, up: 0, down: 0 };
			cl[ip].conns = cl[ip].conns + 1;
			cl[ip].up = cl[ip].up + du;
			cl[ip].down = cl[ip].down + dd;
			cl[ip].exits[name] = (cl[ip].exits[name] || 0) + 1;
		}
	}
	for (let name in ex) {
		let e = ex[name];
		e.clients = length(keys(e.ips));
		delete e.ips;
		// 第一次（没有上一份）算不出这段时间的流量
		if (pts == null) {
			e.up = null;
			e.down = null;
		}
	}
	if (pts == null)
		for (let ip in cl) {
			cl[ip].up = null;
			cl[ip].down = null;
		}
	oc.exits = ex;
	oc.clients = cl;
	oc.conn_dt = (pts != null) ? out.now - pts : null;
	fs.writefile(D + '/conns.prev', sprintf('%J', { t: out.now, c: cur }));
}

function proxies(oc, port) {
	let r = api(port, '/proxies', 5);
	if (r.data == null)
		return;
	oc.proxies = [];
	let all = r.data.proxies || {};
	for (let name in all) {
		let p = all[name];
		if (SKIP[p.type] || type(p.all) == 'array')
			continue;
		let h = p.history || [];
		let last = (length(h) > 0) ? h[length(h) - 1] : null;
		push(oc.proxies, { name: name, type: p.type, alive: p.alive, delay: last ? last.delay : null, at: last ? last.time : null });
	}
}

// 出口名按字节百分号编码，放进 /proxies/<名字>/delay
function enc(s) {
	let r = '';
	for (let i = 0; i < length(s); i++) {
		let c = substr(s, i, 1);
		r += match(c, /^[A-Za-z0-9._~-]$/) ? c : sprintf('%%%02X', ord(s, i));
	}
	return r;
}

// 并发测一批出口（各自经 /proxies/<名字>/delay、各一条连接、各 5 秒时限）：通的写进 results，返回 [通的, 没回话的]。
// mihomo 测不通也会回 JSON（{"message": …}）；什么都没回（API 没响应）的不算测过
function delay_batch(port, names, results) {
	let cmd = [];
	for (let i, n in names) {
		fs.unlink(D + '/delay.' + i);
		push(cmd, sprintf("curl -sS -m 7 -H @%s -o %s %s 2>/dev/null &", D + '/auth', D + '/delay.' + i,
			q('http://127.0.0.1:' + port + '/proxies/' + enc(n) + '/delay?url=' + DELAY_URL + '&timeout=5000')));
	}
	run(join(' ', cmd) + ' wait');
	let ok = [], silent = [];
	for (let i, n in names) {
		let d = null;
		try {
			d = json(fs.readfile(D + '/delay.' + i) || 'null');
		}
		catch (e) {}
		fs.unlink(D + '/delay.' + i);
		if (type(d) != 'object')
			push(silent, n);
		else if (type(d.delay) == 'int' && d.delay > 0) {
			results[n] = d.delay;
			push(ok, n);
		}
	}
	return [ok, silent];
}

// 测一次全部出口：分批，每批最多 BATCH 个并发（不用 /group/GLOBAL/delay：它让全部出口同一时刻经家宽各建一条跨境连接、
// 共用一个时限，挤在一起丢一两个握手包就超时）。fleet 出口排在前面，各自起点按分钟轮换（超预算时不总是同几个轮不上）。
// 整批测完没通的 fleet 出口不超过 RETEST_MAX 个时当场单独再测一次（名额有剩的给其余没通的出口），通了就算这一轮通；
// 没通的 fleet 出口太多多半是路由自己的网络断了，不重测；客户自己的死节点不占名额、也不挡 fleet 出口的重测。
// 时间预算：开测 BUDGET 秒后不再开新的一批，重测也只在 RETEST_BUDGET 秒内开始（中控给这次执行 45 秒）；
// 没轮到的、mihomo 没回话的记进 untested（中控沿用上一次的结果，不算不通）。results 只有通的出口，同 mihomo 分组测试的格式
const BATCH = 6, RETEST_MAX = 5, BUDGET = 22, RETEST_BUDGET = 28;

function rotated(a) {
	let n = length(a), k = (n > 0) ? int(time() / 60) % n : 0, r = [];
	for (let i = 0; i < n; i++)
		push(r, a[(i + k) % n]);
	return r;
}

function delay(oc, port) {
	let all = map(oc.proxies, (p) => p.name);
	let names = rotated(filter(all, (x) => MANAGED[x]));
	for (let x in rotated(filter(all, (x) => !MANAGED[x])))
		push(names, x);
	let n = length(names), results = {}, untested = [], t0 = time(), i = 0;
	for (; i < n && time() - t0 < BUDGET; i += BATCH)
		for (let x in delay_batch(port, slice(names, i, i + BATCH), results)[1])
			push(untested, x);
	for (let x in slice(names, i))
		push(untested, x);
	let failed = filter(names, (x) => results[x] == null && index(untested, x) < 0);
	let fm = filter(failed, (x) => MANAGED[x]), fo = filter(failed, (x) => !MANAGED[x]), retry = [];
	if (length(fm) > 0 && length(fm) <= RETEST_MAX) {
		retry = fm;
		for (let x in fo)
			if (length(retry) < RETEST_MAX)
				push(retry, x);
	}
	else if (length(fm) == 0 && length(fo) <= RETEST_MAX)
		retry = fo;
	oc.delay = { ok: true, results: results, untested: untested };
	if (length(retry) > 0 && time() - t0 < RETEST_BUDGET) {
		let r = delay_batch(port, retry, results);
		oc.delay.retest = { tried: retry, ok: r[0] };
	}
}

function openclash() {
	let oc = { enabled: uci('enable') == '1', mode: uci('en_mode'), config_path: uci('config_path') };
	out.oc = oc;
	let pid = words(run('pidof clash 2>/dev/null || pidof mihomo 2>/dev/null'))[0];
	oc.running = (pid != null);
	if (pid != null) {
		let m = match(fs.readfile('/proc/' + pid + '/status') || '', /VmRSS:[ \t]+([0-9]+)/);
		if (m)
			oc.rss = int(m[1]) * 1024;
	}
	if (oc.config_path) {
		let st = fs.stat(oc.config_path);
		if (st) {
			oc.config_mtime = st.mtime;
			oc.config_sha256 = words(run('sha256sum ' + q(oc.config_path) + ' 2>/dev/null'))[0];
		}
	}
	let port = uci('cn_port');
	if (!oc.running || port == null || !match(port, /^[0-9]+$/))
		return;
	let secret = uci('dashboard_password');
	fs.writefile(D + '/auth', (secret != null) ? 'Authorization: Bearer ' + secret + '\n' : '');
	let v = api(port, '/version', 5);
	if (v.data == null) {
		oc.api_error = v.error;
		return;
	}
	oc.version = v.data.version;
	connections(oc, port);
	proxies(oc, port);
}

// 只测延迟（FLEET_MODE=delay）：OpenClash 没在跑、API 调不通时 delay.ok = false 并写明原因（中控不算出口不通）
function delay_only() {
	let oc = { delay: { ok: false } };
	out.oc = oc;
	let port = uci('cn_port');
	if (!words(run('pidof clash 2>/dev/null || pidof mihomo 2>/dev/null'))[0] || port == null || !match(port, /^[0-9]+$/)) {
		oc.delay.error = 'OpenClash 没在运行';
		return;
	}
	let secret = uci('dashboard_password');
	fs.writefile(D + '/auth', (secret != null) ? 'Authorization: Bearer ' + secret + '\n' : '');
	proxies(oc, port);
	if (oc.proxies == null) {
		oc.delay.error = 'mihomo 的 API 调不通';
		return;
	}
	if (DELAY_URL)
		delay(oc, port);
	delete oc.proxies;
}

function clients() {
	out.leases = [];
	for (let l in lines(fs.readfile('/tmp/dhcp.leases'))) {
		let f = words(l);
		if (length(f) >= 4)
			push(out.leases, [f[2], f[1], (f[3] == '*') ? null : f[3]]);
	}
	out.neigh = [];
	for (let l in lines(run('ip -4 neigh show 2>/dev/null'))) {
		let f = words(l);
		if (length(f) < 2)
			continue;
		let n = [f[0], null, null, f[length(f) - 1]];
		for (let i = 1; i < length(f) - 1; i++) {
			if (f[i] == 'dev')
				n[1] = f[i + 1];
			else if (f[i] == 'lladdr')
				n[2] = f[i + 1];
		}
		push(out.neigh, n);
	}
}

for (let step in ((MODE == 'delay') ? [delay_only] : [system, ifaces, openclash, clients])) {
	try {
		step();
	}
	catch (e) {
		out.error = (out.error ? out.error + '；' : '') + e.message;
	}
}
printf('%J\n', out);
