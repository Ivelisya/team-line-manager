// 升级 agent 前的确认（卡片 / 右键菜单、抽屉「管理」共用外壳里的一份）：agent 会重启、作业开始后不能取消；
// 线路机另说明可能重启一次 Xray（只在 DNS 是链路本地地址时，见 API.md upgrade_agent 的 xray_restarted）。
import { useMachine, useUpgradeAgent } from '@/lib/queries';
import { closeUpgrade, useUpgradeTarget } from '@/lib/nodeActions';
import { useHeld } from '@/lib/useHeld';
import type { MachineView } from '@/lib/types';
import { InfoTip } from '@/components/ui/info-tip';
import { toast } from '@/components/ui/toast';
import { ConfirmDialog } from './dialogs';
import { Note } from './primitives';

const tip = (relay: boolean) => `安装中控上的当前版本并重启 agent。新版本没起来、2 分钟内没有上报${relay ? '，或代理不正常' : ''}，会自动换回旧版本\n`
  + `作业开始后不能取消${relay ? '' : '\n不影响线路上的连接'}`;
const XRAY = '这台线路机的 DNS 是链路本地地址（如 169.254.169.254，常见于 GCE、Oracle 镜像）时，升级会换成公共 DNS 并重启一次 Xray，客户连接会断开，客户端会自动重连\n其他线路机不会重启 Xray';

function UpgradeDialog({ m, open, onOpenChange }: { m: MachineView; open: boolean; onOpenChange: (o: boolean) => void }) {
  const up = useUpgradeAgent();
  const relay = m.kind === 'relay';
  return (
    <ConfirmDialog open={open} onOpenChange={onOpenChange} title={`升级 agent · ${m.name}`} confirmLabel="开始升级" confirmDisabled={!!m.active_job}
      description={<>agent 会重启，开始后不能取消 <InfoTip text={tip(relay)} /></>}
      onConfirm={async () => { await up.mutateAsync(m.id); toast.ok(`已提交「${m.name}」的升级作业`); }}>
      {relay && <Note tone="warn">可能重启一次 Xray，客户连接会断开 <InfoTip text={XRAY} /></Note>}
    </ConfirmDialog>
  );
}

/** 外壳里的一份：目标机器从缓存取，关闭动画期间沿用 */
export function UpgradeHost() {
  const id = useUpgradeTarget();
  const m = useHeld(id, useMachine(id).data);
  return m ? <UpgradeDialog m={m} open={!!id} onOpenChange={(o) => !o && closeUpgrade()} /> : null;
}
