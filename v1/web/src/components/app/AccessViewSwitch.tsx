// 访问监控的两个视图：节点行为（/access）与访问规则（/access/policy）。两个页面的页头共用这一个分段切换。
import { useNavigate } from 'react-router-dom';
import { Segmented } from '@/components/ui/segmented';

export function AccessViewSwitch({ value }: { value: 'behavior' | 'policy' }) {
  const nav = useNavigate();
  return (
    <Segmented label="视图" value={value} onChange={(v) => nav(v === 'policy' ? '/access/policy' : '/access')}
      items={[{ value: 'behavior', label: '节点行为' }, { value: 'policy', label: '访问规则' }]} />
  );
}
