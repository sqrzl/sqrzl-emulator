import { Page, PageHeader } from '@askrjs/themes/components';
import { MailboxTable } from '@/features/mailboxes';

export default function Mailboxes() {
  return (
    <Page>
      <PageHeader
        data-sqrzl-slot="storage-page-header"
        title="Mailboxes"
        description="Search recipient mailboxes and open one to inspect captured messages."
      />

      <MailboxTable />
    </Page>
  );
}
