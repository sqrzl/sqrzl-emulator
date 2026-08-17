import { Link } from '@askrjs/askr/router';
import { ArrowLeftIcon } from '@askrjs/lucide';
import { Button, Page, PageHeader } from '@askrjs/themes/components';
import TextMessageDetails from '../../../components/text/text-message-details';
import { textConversationPath } from '../../../shared/routes';

export default function TextMessagePage({
  peer,
  messageId,
}: {
  peer: string;
  messageId: string;
}) {
  return (
    <Page>
      <PageHeader
        data-sqrzl-slot="storage-page-header"
        title={`Text ${messageId}`}
        description="Inspect provider metadata, media, delivery state, and callback attempt history."
        actions={
          <Button variant="secondary" asChild>
            <Link href={textConversationPath(peer)}>
              <ArrowLeftIcon aria-hidden="true" /> Back to conversation
            </Link>
          </Button>
        }
      />
      <TextMessageDetails peer={peer} messageId={messageId} />
    </Page>
  );
}
