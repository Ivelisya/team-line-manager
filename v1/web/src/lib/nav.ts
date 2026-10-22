// 全站导航：侧栏与命令面板共用这一份（顺序、名称、图标、搜索关键词）。
import { BellRing, Briefcase, CloudUpload, History, LayoutGrid, ListChecks, Radar, Server, Settings, Waypoints, type LucideIcon } from 'lucide-react';

/** end：路径完全相同才算当前页（首页）；badge：侧栏上显示进行中的告警数；kw：命令面板的搜索关键词 */
export interface NavItem { to: string; label: string; icon: LucideIcon; kw: string; end?: boolean; badge?: boolean }

export const NAV: NavItem[] = [
  { to: '/', label: '首页', icon: LayoutGrid, end: true, kw: 'home dashboard 总览' },
  { to: '/nodes', label: '机器', icon: Server, kw: 'nodes 节点 线路机 表格 列表' },
  { to: '/relays', label: '线路', icon: Waypoints, kw: 'relays 线路机 入口' },
  { to: '/provision', label: '上架', icon: CloudUpload, kw: 'provision 添加 新增' },
  { to: '/jobs', label: '作业', icon: ListChecks, kw: 'jobs 任务' },
  { to: '/access', label: '访问监控', icon: Radar, kw: 'access 访问 监控' },
  { to: '/catalog', label: '团队与套餐', icon: Briefcase, kw: 'catalog teams plans providers 目录 团队 套餐 供应商' },
  { to: '/incidents', label: '告警', icon: BellRing, badge: true, kw: 'incidents alerts 静默' },
  { to: '/audit', label: '操作记录', icon: History, kw: 'audit log 审计 日志 登录 执行' },
  { to: '/settings', label: '设置', icon: Settings, kw: 'settings api 密钥 通知 key notify' },
];
