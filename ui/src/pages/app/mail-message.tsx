import { Link } from '@askrjs/askr/router';
import { ArrowLeftIcon } from '@askrjs/lucide';
import { Button, Inline, Stack } from '@askrjs/themes/components';
import MailMessageDetails from '../../components/mail/mail-message-details';
import StoragePageHeader from '../../components/storage/storage-page-header';
import { mailboxPath } from '../../shared/routes';

export default function MailMessagePage({
  mailboxName,
  messageId,
}: {
  mailboxName: string;
  messageId: string;
}) {
  return (
    <Stack gap="4">
      <StoragePageHeader
        title={`Message ${messageId}`}
        description="Inspect message headers, body, and attachment contents."
        actions={
          <Inline gap="2" wrap>
            <Button variant="secondary" asChild>
              <Link href={mailboxPath(mailboxName)}>
                <ArrowLeftIcon aria-hidden="true" /> Back to mailbox
              </Link>
            </Button>
          </Inline>
        }
      />

      <MailMessageDetails mailboxName={mailboxName} messageId={messageId} />
    </Stack>
  );
}
