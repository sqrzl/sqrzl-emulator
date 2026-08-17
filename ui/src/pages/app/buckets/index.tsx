import { Page, PageHeader } from '@askrjs/themes/components';
import { BucketModal, BucketTable } from '@/features/buckets';

export default function Buckets() {
  return (
    <Page>
      <PageHeader
        data-sqrzl-slot="storage-page-header"
        title="Buckets"
        description="Search buckets and open one to browse blobs."
        actions={<BucketModal />}
      />
      <BucketTable />
    </Page>
  );
}
