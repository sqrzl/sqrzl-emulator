import { Link } from '@askrjs/askr/router';
import { Button, Stack } from '@askrjs/themes/components';
import { adminBucketsPath } from '../../shared/routes';
import MailboxTable from '../../components/mail/mailbox-table';
import StoragePageHeader from '../../components/storage/storage-page-header';

export default function Mailboxes() {
  return (
    <Stack gap="4">
      <StoragePageHeader
        title="Mailboxes"
        description="Search recipient mailboxes and open one to inspect captured messages."
        actions={
          <Button variant="secondary" asChild>
            <Link href={adminBucketsPath()}>Back to buckets</Link>
          </Button>
        }
      />

      <MailboxTable />
    </Stack>
  );
}
