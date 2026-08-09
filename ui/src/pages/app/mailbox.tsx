import { navigate } from '@askrjs/askr/router';
import { ArrowLeftIcon } from '@askrjs/lucide';
import { Button, Inline, Stack } from '@askrjs/themes/components';
import MessageTable from '../../components/mail/message-table';
import StoragePageHeader from '../../components/storage/storage-page-header';
import { adminMailboxesPath } from '../../shared/routes';

export default function Mailbox({ mailboxName }: { mailboxName: string }) {
  return (
    <Stack gap="4">
      <StoragePageHeader
        title={`Mailbox ${mailboxName}`}
        description="Inspect messages captured for this mailbox."
        actions={
          <Inline gap="2">
            <Button
              variant="secondary"
              onPress={() => navigate(adminMailboxesPath())}
            >
              <ArrowLeftIcon aria-hidden="true" />
              Back to mailboxes
            </Button>
          </Inline>
        }
      />

      <MessageTable mailboxName={mailboxName} />
    </Stack>
  );
}
