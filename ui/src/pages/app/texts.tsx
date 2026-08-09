import { Inline, Stack } from '@askrjs/themes/components';
import TextConversationTable from '../../components/text/text-conversation-table';
import TextDestinationDialog from '../../components/text/text-destination-dialog';
import TextSimulationDialog from '../../components/text/text-simulation-dialog';
import StoragePageHeader from '../../components/storage/storage-page-header';

export default function TextsPage() {
  return <Stack gap="4">
    <StoragePageHeader
      title="Texts"
      description="Inspect outbound SMS/MMS, simulate inbound provider events, and replay callbacks."
      actions={<Inline gap="2" wrap><TextDestinationDialog /><TextSimulationDialog /></Inline>}
    />
    <TextConversationTable />
  </Stack>;
}
