// 全站导航：侧栏与命令面板共用这一份（顺序、名称、图标、搜索关键词）。
import { BellRing, Briefcase, ChartColumnStacked, CloudUpload, History, LayoutGrid, ListChecks, Radar, Router, Server, Settings, Users, Waypoints, type LucideIcon } from 'lucide-react';

/** end：路径完全相同才算当前页（首页）；badge：侧栏上显示进行中的告警数；kw：命令面板的搜索关键词 */
export interface NavItem { to: string; label: string; icon: LucideIcon; kw: string; end?: boolean; badge?: boolean }

export const NAV: NavItem[] = [
  { to: '/', label: '首页', icon: LayoutGrid, end: true, kw: 'home dashboard 总览' },
  { to: '/nodes', label: '机器', icon: Server, kw: 'nodes 节点 线路机 表格 列表' },
  { to: '/relays', label: '线路', icon: Waypoints, kw: 'relays 线路机 入口' },
  { to: '/routers', label: '软路由', icon: Router, kw: 'routers router openwrt openclash 软路由 路由器 客户现场 出口 接入' },
  { to: '/provision', label: '上架', icon: CloudUpload, kw: 'provision 添加 新增' },
  { to: '/jobs', label: '作业', icon: ListChecks, kw: 'jobs 任务' },
  { to: '/traffic', label: '流量', icon: ChartColumnStacked, kw: 'traffic 流量 每日 统计 下载 上传 计费 daily' },
  { to: '/access', label: '访问监控', icon: Radar, kw: 'access 访问 监控' },
  { to: '/providers', label: '供应商', icon: Briefcase, kw: 'providers 供应商 厂商 套餐 续费 到期 付款 due' },
  { to: '/teams', label: '团队', icon: Users, kw: 'teams 团队 管理 台账 计费 收款 客户页' },
  { to: '/incidents', label: '告警', icon: BellRing, badge: true, kw: 'incidents alerts 静默 断线' },
  { to: '/audit', label: '操作记录', icon: History, kw: 'audit log 审计 日志 登录 执行' },
  { to: '/settings', label: '设置', icon: Settings, kw: 'settings api 密钥 通知 月报 key notify 账户 安全 两步验证 2fa totp 密码 password 会话 session' },
];
