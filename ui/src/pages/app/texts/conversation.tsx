import { Link } from '@askrjs/askr/router';
import { ArrowLeftIcon } from '@askrjs/lucide';
import { Button, Page, PageHeader } from '@askrjs/themes/components';
import { MessageTable } from '@/features/text-messages';
import { adminTextsPath } from '@/shared/routes';

export default function TextConversationPage({ peer }: { peer: string }) {
  return (
    <Page>
      <PageHeader
        data-sqrzl-slot="storage-page-header"
        title={`Conversation ${peer}`}
        description="Review inbound and outbound messages in canonical timestamp order."
        actions={
          <Button variant="secondary" asChild>
            <Link href={adminTextsPath()}>
              <ArrowLeftIcon aria-hidden="true" /> Back to texts
            </Link>
          </Button>
        }
      />
      <MessageTable peer={peer} />
    </Page>
  );
}
