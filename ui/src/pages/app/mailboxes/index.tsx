import { Link } from '@askrjs/askr/router';
import { Button, Page, PageHeader } from '@askrjs/themes/components';
import { adminBucketsPath } from '@/shared/routes';
import { MailboxTable } from '@/features/mailboxes';

export default function Mailboxes() {
  return (
    <Page>
      <PageHeader
        data-sqrzl-slot="storage-page-header"
        title="Mailboxes"
        description="Search recipient mailboxes and open one to inspect captured messages."
        actions={
          <Button variant="secondary" asChild>
            <Link href={adminBucketsPath()}>Back to buckets</Link>
          </Button>
        }
      />

      <MailboxTable />
    </Page>
  );
}
