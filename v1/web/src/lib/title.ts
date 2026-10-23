// 标签页标题只在这里拼：「(进行中的告警数) 抽屉里的机器名 · 团队线路管理」。顶栏报告警数、抽屉报机器名，各写各的那一段，
// 谁先谁后都拼得对；卸载时清掉自己那段（关上抽屉回到站名，退出登录不留告警数）
import { useEffect } from 'react';

const APP = '团队线路管理';
let alerts = 0;
let name = '';
const apply = () => { document.title = `${alerts > 0 ? `(${alerts}) ` : ''}${name ? `${name} · ` : ''}${APP}`; };

/** 顶栏：进行中、还没确认的告警条数（同告警胶囊），0 不显示 */
export function useTitleAlerts(n: number) {
  useEffect(() => { alerts = n; apply(); return () => { alerts = 0; apply(); }; }, [n]);
}
/** 抽屉：打开的机器名；关着、还没加载出来时为空 */
export function useTitleName(v: string | undefined) {
  useEffect(() => { name = v ?? ''; apply(); return () => { name = ''; apply(); }; }, [v]);
}
