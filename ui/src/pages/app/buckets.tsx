import { Inline, Stack } from '@askrjs/themes/layouts';
import BucketModal from '../../components/storage/bucket-modal';
import BucketTable from '../../components/storage/bucket-table';

export default function Buckets() {
  return (
    <Stack gap="4">
      <Inline justify="between" align="center" gap="3" wrap="wrap">
        <Stack gap="1">
          <h1>Buckets</h1>
          <p>Manage buckets and open their blob listings.</p>
        </Stack>
        <BucketModal />
      </Inline>

      <BucketTable />
    </Stack>
  );
}
