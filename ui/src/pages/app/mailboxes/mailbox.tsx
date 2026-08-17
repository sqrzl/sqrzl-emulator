import { navigate } from '@askrjs/askr/router';
import { ArrowLeftIcon } from '@askrjs/lucide';
import { Block, Button, Page, PageHeader } from '@askrjs/themes/components';
import MessageTable from '../../../components/mail/message-table';
import { adminMailboxesPath } from '../../../shared/routes';

export default function Mailbox({ mailboxName }: { mailboxName: string }) {
  return (
    <Page>
      <PageHeader
        data-sqrzl-slot="storage-page-header"
        title={`Mailbox ${mailboxName}`}
        description="Inspect messages captured for this mailbox."
        actions={
          <Block direction="row" gap="xs">
            <Button
              variant="secondary"
              onPress={() => navigate(adminMailboxesPath())}
            >
              <ArrowLeftIcon aria-hidden="true" />
              Back to mailboxes
            </Button>
          </Block>
        }
      />

      <MessageTable mailboxName={mailboxName} />
    </Page>
  );
}
