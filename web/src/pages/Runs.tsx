import InventoryPage from '@/components/InventoryPage';
import RunHistoryTable from '@/components/RunHistoryTable';

export default function Runs() {
  return (
    <InventoryPage title="执行记录" status="ready">
      <RunHistoryTable />
    </InventoryPage>
  );
}
