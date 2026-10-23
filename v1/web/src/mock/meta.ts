// mock 的 GET /meta：值抄自服务端 crates/fleet-server/src/meta.rs（那边改了这里跟着改），外加订阅里服务端算的几项
import type { Meta, Subscription } from '@/lib/types';
import { addMonths } from '@/lib/format';
import { toUsd, usdPerUnit } from './fx';

export const META: Meta = {
  org_timezone: 'Asia/Shanghai', org_timezone_label: '北京时间', org_utc_offset_s: 28800,
  timezones: ['Asia/Shanghai', 'Asia/Hong_Kong', 'Asia/Taipei', 'Asia/Tokyo', 'Asia/Seoul', 'Asia/Singapore',
    'Europe/London', 'Europe/Berlin', 'Europe/Amsterdam', 'America/Los_Angeles', 'America/Chicago', 'America/New_York', 'UTC'],
  currencies: ['CNY', 'USD', 'EUR', 'HKD', 'JPY', 'GBP'],
  cycles: [{ key: 'monthly', label: '月', months: 1 }, { key: 'quarterly', label: '季', months: 3 }, { key: 'semiannual', label: '半年', months: 6 }, { key: 'yearly', label: '年', months: 12 }],
  kinds: { vps: 'VPS 节点', proxy: '家宽代理', relay: '线路机' }, ip_node_label: '同机 IP 节点',
  lifecycles: { draft: '草稿', provisioning: '上架中', active: '运行中', retiring: '退役中', retired: '已退役', failed: '上架失败' },
  healths: { ok: '正常', degraded: '降级', down: '故障', unknown: '未知', maintenance: '维护', inactive: '未启用' },
  job_kinds: {
    provision_vps: '上架 VPS 节点', provision_proxy: '上架家宽代理', provision_relay: '上架线路机', provision_ip: '上架同机 IP 节点',
    attach_entry: '添加入口', move_entry: '换线路机', restore_vps: '恢复 VPS 节点', restore_proxy: '恢复家宽代理', restore_relay: '恢复线路机', restore_ip: '恢复同机 IP 节点',
    retire: '退役', purge: '彻底删除', upgrade_agent: '升级 agent', replace: '替换节点', tune: '系统调优',
  },
  job_states: { queued: '排队中', running: '进行中', waiting: '等待中', cancelling: '取消中', succeeded: '已完成', failed: '失败', cancelled: '已取消' },
  step_states: { pending: '等待', running: '进行中', done: '已完成', skipped: '已跳过', failed: '失败', compensated: '已回滚' },
  entry_states: { pending: '应用中', active: '运行中', disabled: '已停用', removing: '移除中', removed: '已移除' },
  traffic_rules: { min: '取下行和上行中较小的', max: '取下行和上行中较大的', sum: '下行加上行', out: '只算上行' },
  traffic_resets: { month: '自然月', billing: '账单日', rolling30: '滚动 30 天' },
  due_states: { ok: '未到期', soon: '即将到期', overdue: '已过期' },
  policy_sync_states: { applied: '已应用', applying: '进行中', failed: '失败', gone: '已退役' },
  policy_modes: { observe: '仅观察', allowlist: '白名单', denylist: '黑名单', blocked: '暂停新连接', inherit: '跟随团队' },
  rules: {
    agent_offline: 'agent 离线', clock_skew: 'agent 时钟不准', no_entry: '没有在用入口', egress_fail: '出口不通', tcp_fail: '线路机到节点不通', renewal_due: '即将到期', renewal_overdue: '已过期',
    relay_apply_lag: '配置未应用', relay_apply_error: '配置应用失败', relay_xray_down: 'Xray 未运行', relay_unreachable: '客户入口连不上', host_cpu_high: 'CPU 持续偏高', host_mem_high: '内存持续偏高', host_disk_high: '磁盘空间不足',
    traffic_quota_high: '流量额度吃紧', ipintel_new_isp: '来源出现新运营商', ipintel_new_city: '来源出现新城市', ipintel_datacenter_source: '机房 / VPN 来源', policy_blocked: '访问规则暂停',
    central_channel_failing: '通知渠道发送失败', central_backup_stale: '备份过期', central_disk_low: '中控磁盘空间不足', central_ingress: 'agent 同时失联', central_view_stuck: '机器视图算不出来', central_tunnel_host: '软路由回连地址不是中控',
    router_offline: '软路由离线', router_collect_error: '软路由读取出错', router_openclash_down: 'OpenClash 没在运行', router_exits_down: '软路由出口不通', router_wan_down: '软路由 WAN 口断开', router_resource_high: '软路由资源吃紧',
  },
  severities: { critical: { label: '严重', tone: 'bad' }, warning: { label: '警告', tone: 'warn' }, info: { label: '提示', tone: 'info' } },
  thresholds: {
    traffic_quota_warn_pct: 90, traffic_quota_over_pct: 100,
    usage: { cpu: { warn: 75, bad: 90 }, mem: { warn: 75, bad: 90 }, disk: { warn: 75, bad: 85 }, load: { warn: 75, bad: 90 } },
  },
  defaults: { currency: 'USD', cycle: 'monthly', remind_days: 7, traffic_rule: 'max', traffic_reset: 'month', ss_method: 'aes-256-gcm', ss_port: 8388 },
  terminal: { per_user: 3, global: 8, idle_s: 900, max_age_s: 7200, max_message: 65536, max_dim: 500, ticket_ttl_s: 60, verify_ttl_s: 300 },
  ssh_public_key: 'ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIMockControlPublicKeyForTheFleetDemo0000 fleet@control',
};

const DAY = 86400_000;
export const cycleMonths = (c: string) => META.cycles.find((x) => x.key === c)?.months ?? 1;
/** 订阅里服务端算的几项（同 meta.rs 的 due_state / monthly_cents 与视图的 next_due / period_days、USD 折算），due_days 先算好 */
export function subCalc(s: Subscription): Subscription {
  const n = cycleMonths(s.cycle);
  s.due_state = s.due_days < 0 && !(s.auto_renew && s.due_days === -1) ? 'overdue' : s.due_days <= s.remind_days && !s.auto_renew ? 'soon' : 'ok';
  s.monthly_cents = Math.round((s.price_cents ?? 0) / n);
  // 折成 USD（同服务端 fx.rs）：没有汇率时都是 null
  s.usd_per_unit = usdPerUnit(s.currency); s.monthly_usd_cents = toUsd(s.monthly_cents, s.currency); s.price_usd_cents = toUsd(s.price_cents ?? 0, s.currency);
  s.next_due = addMonths(s.due_date, n);
  s.period_days = Math.round((Date.parse(s.due_date) - Date.parse(addMonths(s.due_date, -n))) / DAY);
  return s;
}
