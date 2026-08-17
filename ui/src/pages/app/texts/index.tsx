import { Block, Page, PageHeader } from '@askrjs/themes/components';
import { ConversationTable } from '@/features/text-conversations';
import { DestinationDialog, SimulationDialog } from '@/features/texts';

export default function TextsPage() {
  return (
    <Page>
      <PageHeader
        data-sqrzl-slot="storage-page-header"
        title="Texts"
        description="Inspect outbound SMS/MMS, simulate inbound provider events, and replay callbacks."
        actions={
          <Block direction="row" gap="xs" style={{ flexWrap: 'wrap' }}>
            <DestinationDialog />
            <SimulationDialog />
          </Block>
        }
      />
      <ConversationTable />
    </Page>
  );
}
