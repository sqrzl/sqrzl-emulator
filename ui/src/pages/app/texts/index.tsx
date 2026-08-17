import { Block, Page, PageHeader } from '@askrjs/themes/components';
import TextConversationTable from '../../../components/text/text-conversation-table';
import TextDestinationDialog from '../../../components/text/text-destination-dialog';
import TextSimulationDialog from '../../../components/text/text-simulation-dialog';

export default function TextsPage() {
  return (
    <Page>
      <PageHeader
        data-sqrzl-slot="storage-page-header"
        title="Texts"
        description="Inspect outbound SMS/MMS, simulate inbound provider events, and replay callbacks."
        actions={
          <Block direction="row" gap="xs" style={{ flexWrap: 'wrap' }}>
            <TextDestinationDialog />
            <TextSimulationDialog />
          </Block>
        }
      />
      <TextConversationTable />
    </Page>
  );
}
