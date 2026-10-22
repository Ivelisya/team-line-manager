// 作业的展示辅助：kind 中文名、state 分类、步骤输出摘要。取值以 API.md「返回形状补充」为准。
import type { EntryLifecycle, JobStep, JobSummary } from './types';
import { ENTRY_LABEL } from './sort';
import { dtFull } from './format';

const JOB_KIND: Record<string, string> = {
  provision_vps: '上架落地机', provision_proxy: '上架家宽代理', provision_relay: '上架线路机', provision_ip: '上架同机 IP 节点',
  attach_entry: '新增入口', move_entry: '换线路机', restore_vps: '恢复落地机', restore_proxy: '恢复家宽代理', restore_relay: '恢复线路机', restore_ip: '恢复同机 IP 节点',
  retire: '退役', purge: '彻底删除', upgrade_agent: '升级 agent',
};
export const jobKindLabel = (k: string) => JOB_KIND[k] ?? k;

/** 进行中：还没到终态 */
export const jobLive = (state: string) => ['queued', 'running', 'waiting', 'cancelling'].includes(state);

/** 步骤的展示类别（步骤条、时间线共用）。服务端的步骤状态：pending / running / done / skipped / failed / compensated
 *  （已补偿：作业取消或失败后，做过的步骤被逆序撤销）；不认识的按“等待”显示 */
export type StepKind = 'done' | 'running' | 'failed' | 'skipped' | 'compensated' | 'pending';
const STEP_KINDS = new Set<string>(['done', 'running', 'failed', 'skipped', 'compensated']);
export const stepKind = (s: JobStep): StepKind => (STEP_KINDS.has(s.state) ? (s.state as StepKind) : 'pending');
export const STEP_KIND_LABEL: Record<StepKind, string> = { done: '已完成', running: '进行中', failed: '失败', skipped: '已跳过', compensated: '已补偿', pending: '等待' };

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
  network: '网络错误', auth: 'SSH 认证失败', host_key: '主机指纹不符', remote: '远端命令失败', timeout: '超时', internal: '中控内部错误',
};
export const errorKindLabel = (k: string) => ERROR_KIND_LABEL[k] ?? k;

// ── 步骤输出 ──
// 服务端各步骤 Outcome::Done(json!{..}) 的全部 key（crates/fleet-server/src/jobs/steps.rs、steps/lifecycle.rs），
// 加上跳过时的 {reason}（jobs/mod.rs）。服务端新增输出 key 时同步补这里。
type OutFmt = (v: unknown) => string;
const text: OutFmt = (v) => String(v);
const unit = (u: string): OutFmt => (v) => `${v} ${u}`;
const yesNo = (y: string, n: string): OutFmt => (v) => (v ? y : n);
// 输出里没有端口号：落地机是 8388，同机 IP 节点是它自己的 SS 端口，统一不写具体端口
const CLEANUP_SS: Record<string, string> = { gone: '已停止', still_listening: '仍在监听 SS 端口' };
/** [中文标签, 取值格式]；null = 内部 id，不显示 */
const OUTPUT: Record<string, [string, OutFmt] | null> = {
  // preflight
  host_key: ['主机指纹', text], arch: ['架构', text], os: ['系统', text], kernel: ['内核', text], mem_mb: ['内存', unit('MB')],
  // ip_check（同机 IP 节点，在宿主机上执行；egress_ip 同出口验收）：local_ips 是逗号加空格分隔的字符串
  ss_port: ['SS 端口', text], local_ips: ['宿主机本机 IP', text],
  // bootstrap
  method: ['登录方式', () => '用 root 密码推送了中控公钥'],
  // converge / converge_relay（同机 IP 节点不做系统基线，bbr 为 null、不显示）
  bbr: ['BBR', yesNo('已启用', '未启用')], nft_relays: ['防火墙放行', text], ssserver_updated: ['ssserver', yesNo('已更新', '已是最新')], xray: ['Xray 版本', text],
  // agent / wait_report（prev_version：这次换下来、留作回滚的旧程序的版本；xray_checks：升级线路机时新版本还要做到的几项）
  binary_updated: ['agent 程序', yesNo('已更新', '已是最新')], token_id: null, prev_version: ['换下的版本', text], xray_checks: ['升级后检查', text],
  first_report_at: ['首次上报', (v) => dtFull(String(v))], agent_version: ['agent 版本', text], old_tokens_revoked: ['撤销旧令牌', unit('枚')],
  // attach / attach_new / detach_old / wait_applied
  entry_id: null, relay_id: null, old_entry_id: null, relay: ['线路机', text], port: ['入口端口', text], generation: ['配置版本', text],
  entry: ['入口状态', (v) => ENTRY_LABEL[v as EntryLifecycle]],
  // verify
  latency_ms: ['出口延迟', (v) => `${Math.round(Number(v))} ms`], egress_ip: ['出口 IP', text],
  // activate（恢复）/ 跳过
  restored: null, reason: ['说明', text],
  // detach / cleanup / revoke / finalize / purge / wait_detached_old
  entries: ['入口', unit('个')], ss: ['ssserver', (v) => CLEANUP_SS[String(v)]], revoked: ['撤销令牌', unit('枚')], renumbered: ['重新编号', unit('台')],
  rows_deleted: ['删除数据', unit('行')], already_gone: ['状态', () => '目标已不存在，无需处理'],
};

/** 步骤输出（JSON 对象）→ 每行 “标签：值”；空值与内部 id 不显示 */
export function outputLines(o: JobStep['output']): string[] {
  if (o == null) return [];
  return Object.entries(o).flatMap(([k, v]) => {
    const m = OUTPUT[k];
    if (m === null || v == null || v === '') return [];
    // 未收录的 key（服务端新加了输出）原样显示，提醒补表
    if (m === undefined) return [`${k}：${typeof v === 'object' ? JSON.stringify(v) : String(v)}`];
    return [`${m[0]}：${m[1](v)}`];
  });
}
