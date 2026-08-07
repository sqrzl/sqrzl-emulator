import { Link } from '@askrjs/askr/router';
import { ArrowLeftIcon } from '@askrjs/lucide';
import { Button, Stack } from '@askrjs/themes/components';
import TextMessageTable from '../../components/text/text-message-table';
import StoragePageHeader from '../../components/storage/storage-page-header';
import { adminTextsPath } from '../../shared/routes';

export default function TextConversationPage({ peer }: { peer: string }) {
  return <Stack gap="4">
    <StoragePageHeader
      title={`Conversation ${peer}`}
      description="Review inbound and outbound messages in canonical timestamp order."
      actions={<Button variant="secondary" asChild><Link href={adminTextsPath()}><ArrowLeftIcon aria-hidden="true" /> Back to texts</Link></Button>}
    />
    <TextMessageTable peer={peer} />
  </Stack>;
}
