import InventoryPage from '@/components/InventoryPage';
import RunHistoryTable from '@/components/RunHistoryTable';
import InventoryStatistics from '@/components/InventoryStatistics';
import { getRunStatistics } from '@/services/executions';
import { useInventory } from '@/services/useInventory';

export default function Runs() {
  const { state, refresh } = useInventory(getRunStatistics);
  const stats = state.status === 'ready' ? state.data : undefined;
  return (
    <InventoryPage title="执行记录" status="ready" refresh={refresh}>
      <InventoryStatistics items={[
        ['派发中', stats?.running ?? '—'],
        ['已暂停', stats?.paused ?? '—'],
        ['待审批', stats?.awaitingApproval ?? '—'],
        ['派发已结束', stats?.ended ?? '—'],
        ['含异常操作', stats?.exceptionRuns ?? '—'],
      ]} />
      <RunHistoryTable />
    </InventoryPage>
  );
}
