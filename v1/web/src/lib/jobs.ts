// 作业的展示辅助：state 分类、步骤输出摘要。作业类型、步骤状态的中文名在 /meta（derive.jobKindLabel / stepLabel）。
import type { JobStep, JobSummary } from './types';
import { entryLabel } from './derive';
import { dtFull, mem, str } from './format';

export { jobKindLabel } from './derive';

/** 进行中：还没到终态 */
export const jobLive = (state: string) => ['queued', 'running', 'waiting', 'cancelling'].includes(state);

/** 步骤的展示类别（步骤条、时间线共用）。服务端的步骤状态：pending / running / done / skipped / failed / compensated
 *  （已补偿：作业取消或失败后，做过的步骤被逆序撤销）；不认识的按“等待”显示 */
export type StepKind = 'done' | 'running' | 'failed' | 'skipped' | 'compensated' | 'pending';
const STEP_KINDS = new Set<string>(['done', 'running', 'failed', 'skipped', 'compensated']);
export const stepKind = (s: JobStep): StepKind => (STEP_KINDS.has(s.state) ? (s.state as StepKind) : 'pending');

/** 总进度：已走过的步骤（step 之前的，完成或跳过）/ 总步骤；作业成功时一定是 100。列表项与详情通用 */
export function jobProgress(j: JobSummary) {
  const total = j.step_count || 1;
  const done = j.state === 'succeeded' ? total : Math.min(total, j.step);
  return { done, total, pct: (done / total) * 100 };
}
/** 当前（或失败的）步骤标题，进度浮层里显示；排队中为 null */
export function currentStepTitle(j: JobSummary): string | null {
  return j.state === 'queued' ? null : j.step_title;
}

/** error_kind 的中文说明（服务端 ssh.rs ErrKind）；未收录的原样显示 */
const ERROR_KIND_LABEL: Record<string, string> = {
  network: '网络错误', auth: 'SSH 认证失败', host_key: '主机指纹不符', remote: '远端命令失败', timeout: '超时', internal: '中控出错了',
};
/** 同是 remote：升级 agent 换上新版本后的回滚检查（起不来、线路机检查没过）不是远端命令出错，按错误文字区分开 */
export const errorKindLabel = (k: string, error?: string | null) =>
  k === 'remote' && error && /检查没通过/.test(error) ? '升级后检查未通过'
  : k === 'remote' && error && /起不来|没有起来/.test(error) ? 'agent 没起来'
  : ERROR_KIND_LABEL[k] ?? k;

/** 服务端作业文字（步骤输出、错误、日志）里的「代数」是内部说法，界面上统一叫「配置版本」（同线路页的配置条）；
 *  旧记录里防火墙名单写成「期望 [a, b]，实际 []」的原始数组：写成「期望 a、b；实际 无」（同现在的服务端） */
export const jobText = (s: string) => s.replace(/配置应用到代数 (\d+)/g, '配置版本 $1 已应用').replace(/(期望状态|配置)?代数/g, '配置版本')
  .replace(/(期望|实际) \[([^\]]*)\]/g, (_, k: string, v: string) => `${k} ${v.trim() ? v.split(/,\s*/).join('、') : '无'}`).replace(/(期望 [^，；]*)，实际/g, '$1；实际');

// ── 步骤输出 ──
// 服务端各步骤 Outcome::Done(json!{..}) 的全部 key（crates/fleet-server/src/jobs/steps.rs、steps/lifecycle.rs），
// 加上跳过时的 {reason}（jobs/mod.rs）。服务端新增输出 key 时同步补这里。
type OutFmt = (v: unknown) => string;
const text: OutFmt = (v) => String(v);
const unit = (u: string): OutFmt => (v) => `${v} ${u}`;
const yesNo = (y: string, n: string): OutFmt => (v) => (v ? y : n);
// 输出里没有端口号：普通 VPS 节点是 8388，同机 IP 节点是它自己的 SS 端口，统一不写具体端口
const HOST_FW: Record<string, string> = {
  none: '', ufw: 'ufw（已放行 SS 端口）', firewalld: 'firewalld（已放行 SS 端口）', 'iptables-drop': 'iptables 默认丢弃入站，需要手动放行 SS 端口',
  'ufw-error': 'ufw 规则没改成', 'firewalld-error': 'firewalld 规则没改成',
};
const CLEANUP_SS: Record<string, string> = { gone: '已停止', still_listening: '仍在监听 SS 端口' };
/** [中文标签, 取值格式]；null = 内部 id，不显示 */
const OUTPUT: Record<string, [string, OutFmt] | null> = {
  // preflight
  host_key: ['主机指纹', text], host_fw: ['主机防火墙', (v) => HOST_FW[String(v)] ?? String(v)], arch: ['架构', text], os: ['系统', text], kernel: ['内核', text], mem_mb: ['内存', (v) => str(mem(Number(v) * 1024 * 1024))],
  // ip_check（同机 IP 节点，在宿主机上执行；egress_ip 同出口验收）：local_ips 是逗号加空格分隔的字符串；
  // bind_ip 只有 1:1 NAT 映射的才有（公网 IP 映射到的宿主机本地地址），在网卡上的为 null、不显示
  ss_port: ['SS 端口', text], local_ips: ['宿主机本机 IP', text], bind_ip: ['本地地址（NAT）', text],
  // converge 的逐实例结果（同机 IP 节点每个实例一条）与系统调优
  instances: ['SS 实例', text], ip: ['IP', text], restarted: ['ssserver', yesNo('已重启', '没有重启')], xray_restarted: ['Xray', yesNo('已重启', '没有重启')],
  tune: ['系统调优', text], profile: ['调优方案', text], journald: ['journald', text], ntp: ['时间同步', text], nofile: ['文件句柄上限', text], container: ['容器内', yesNo('是', '否')], swap_mb: ['交换分区', (v) => `${v} MB`],
  // 软路由改出口（替换节点、换线路机）
  routers: ['软路由', text], name: ['名称', text], exits: ['出口', text], exit: ['出口', text], to: ['指向', text], backup: ['备份', text], delay_ms: ['延迟（ms）', text], id: null, job: null, batch_leader: null,
  // bootstrap
  method: ['登录方式', () => '用 root 密码装上了中控公钥'],
  // converge / converge_relay（同机 IP 节点不做系统基线，bbr 为 null、不显示）
  bbr: ['BBR', yesNo('已启用', '未启用')], nft_relays: ['防火墙放行', (v) => String(v).split(',').map((s) => s.trim()).filter(Boolean).join('、')], ssserver_updated: ['ssserver', yesNo('已更新', '已是最新')], xray: ['Xray 版本', text],
  // agent / wait_report（prev_version：这次换下来、留作回滚的旧程序的版本；xray_checks：升级线路机时新版本还要做到的几项）
  binary_updated: ['agent 程序', yesNo('已更新', '已是最新')], token_id: null, prev_version: ['换下的版本', text], xray_checks: ['升级后检查', (v) => jobText(String(v))],
  first_report_at: ['首次上报', (v) => dtFull(String(v).replace(/^(\S+) (\S+) \+00:00(:00)?$/, '$1T$2Z'))], agent_version: ['agent 版本', text], old_tokens_revoked: ['作废旧凭据', unit('个')],
  // attach / attach_new / detach_old / wait_applied
  entry_id: null, relay_id: null, old_entry_id: null, relay: ['线路机', text], port: ['入口端口', text], generation: ['配置版本', text],
  entry: ['入口状态', (v) => entryLabel(String(v))],
  // verify
  latency_ms: ['出口延迟', (v) => `${Math.round(Number(v))} ms`], egress_ip: ['出口 IP', text],
  // activate（恢复）/ 跳过
  restored: null, reason: ['说明', text],
  // detach / cleanup / revoke / finalize / purge / wait_detached_old
  entries: ['入口', unit('个')], ss: ['ssserver', (v) => CLEANUP_SS[String(v)]], revoked: ['作废凭据', unit('个')], renumbered: ['重新编号', unit('台')],
  // renumber（替换节点的交换编号）：交换后两台各自的新名字
  new: ['新节点', text], old: ['旧节点', text],
  rows_deleted: ['删除数据', unit('行')], recycle_bin_id: ['回收站编号', (v) => `#${v}`], already_gone: ['状态', () => '目标已不存在，无需处理'],
};

/** 没收录的值写成可读的键值，不露原始 JSON：数组用「、」连，对象写「标签 值，标签 值」（键认得的用中文标签，内部 id 不写），true / false 写是 / 否 */
function plain(v: unknown): string {
  if (Array.isArray(v)) return v.map(plain).join('、');
  if (typeof v === 'boolean') return v ? '是' : '否';
  if (v && typeof v === 'object') {
    return Object.entries(v).filter(([k, x]) => OUTPUT[k] !== null && x != null && x !== '')
      .map(([k, x]) => [OUTPUT[k]?.[0] ?? k, OUTPUT[k]?.[1] && typeof x !== 'object' ? OUTPUT[k]![1](x) : plain(x)]).filter(([, t]) => t !== '').map(([l, t]) => `${l} ${t}`).join('，');
  }
  return String(v);
}

/** 步骤输出（JSON 对象）→ 每行 “标签：值”；空值与内部 id 不显示 */
export function outputLines(o: JobStep['output']): string[] {
  if (o == null) return [];
  return Object.entries(o).flatMap(([k, v]) => {
    const m = OUTPUT[k];
    if (m === null || v == null || v === '') return [];
    // 未收录的 key（服务端新加了输出）照键名写，值写成可读的键值
    if (m === undefined) return [`${k}：${plain(v)}`];
    const out = typeof v === 'object' ? plain(v) : m[1](v);
    return out === '' ? [] : [`${m[0]}：${out}`];
  });
}
