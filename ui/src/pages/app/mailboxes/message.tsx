import { Link } from '@askrjs/askr/router';
import { ArrowLeftIcon } from '@askrjs/lucide';
import { Block, Button, Page, PageHeader } from '@askrjs/themes/components';
import MailMessageDetails from '../../../components/mail/mail-message-details';
import { mailboxPath } from '../../../shared/routes';

export default function MailMessagePage({
  mailboxName,
  messageId,
}: {
  mailboxName: string;
  messageId: string;
}) {
  return (
    <Page>
      <PageHeader
        data-sqrzl-slot="storage-page-header"
        title={`Message ${messageId}`}
        description="Inspect message headers, body, and attachment contents."
        actions={
          <Block direction="row" gap="xs" style={{ flexWrap: 'wrap' }}>
            <Button variant="secondary" asChild>
              <Link href={mailboxPath(mailboxName)}>
                <ArrowLeftIcon aria-hidden="true" /> Back to mailbox
              </Link>
            </Button>
          </Block>
        }
      />

      <MailMessageDetails mailboxName={mailboxName} messageId={messageId} />
    </Page>
  );
}
