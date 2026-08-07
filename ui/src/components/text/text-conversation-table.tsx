import { For } from '@askrjs/askr/control';
import { createMutation } from '@askrjs/askr/data';
import { Link } from '@askrjs/askr/router';
import { TrashIcon } from '@askrjs/lucide';
import { Badge, Button, Inline } from '@askrjs/themes/components';
import { Table, TableBody, TableCell, TableHead, TableHeaderCell, TableRow } from '@askrjs/ui';
import type { TextConversation } from '../../adapters/api.g';
import { useCursorList } from '../../features/storage/use-cursor-list';
import { useDeleteTarget } from '../../features/storage/use-delete-target';
import { textConversationListKey } from '../../features/texts/keys';
import { deleteTextConversation, listTextConversationPage } from '../../features/texts/texts.query';
import { formatRelativeTime } from '../../shared/format';
import { textConversationPath } from '../../shared/routes';
import DataTableSection from '../storage/data-table-section';
import TextDeleteDialog from './text-delete-dialog';

export default function TextConversationTable() {
  const list = useCursorList<TextConversation>(
    textConversationListKey,
    'search',
    ({ next, search, signal }) => listTextConversationPage({ next, search, signal })
  );
  const remove = createMutation({
    action: (target: { id: string }, { signal }) => deleteTextConversation({ peer: target.id, signal }),
    affects: () => [textConversationListKey],
    afterSuccess: 'invalidate',
  });
  const remover = useDeleteTarget<{ id: string }>({
    keyOf: (target) => target.id,
    remove: async (target) => { await remove.execute(target); },
    removeError: 'Conversation could not be deleted.',
  });
  const items = list.items();
  const hasItems = items.length > 0;

  return (
    <>
      <DataTableSection
        searchInputId="text-conversation-search"
        searchLabel="Search text conversations"
        searchValue={list.search()}
        onSearch={list.setSearch}
        loading={list.pending() && !hasItems}
        errored={Boolean(list.error()) && !hasItems}
        empty={!hasItems}
        emptyTitle={list.search() ? 'No conversations match this search' : 'No text conversations yet'}
        emptyDescription={list.search() ? 'Try another phone number or message body.' : 'Send a provider message or simulate an inbound text to begin.'}
        errorTitle="Text conversations could not load"
        errorDescription="Retry the admin API call to see text conversations."
        onRetry={() => list.refresh()}
        hasNext={list.hasNext()}
        hasPrevious={list.hasPrevious()}
        onNext={() => list.next()}
        onPrevious={() => list.previous()}
      >
        <Table>
          <TableHead><TableRow>
            <TableHeaderCell>Peer</TableHeaderCell>
            <TableHeaderCell>Provider</TableHeaderCell>
            <TableHeaderCell>Last message</TableHeaderCell>
            <TableHeaderCell>Updated</TableHeaderCell>
            <TableHeaderCell>Messages</TableHeaderCell>
            <TableHeaderCell><Inline justify="end">Actions</Inline></TableHeaderCell>
          </TableRow></TableHead>
          <TableBody>
            <For each={items} by={(item) => item.peer}>
              {(item) => <TableRow key={item.peer}>
                <TableCell><Link href={textConversationPath(item.peer)}>{item.peer}</Link></TableCell>
                <TableCell><Badge variant="secondary">{item.provider}</Badge></TableCell>
                <TableCell>{item.last_direction === 'inbound' ? '← ' : '→ '}{item.last_message_body || 'No body'}</TableCell>
                <TableCell>{formatRelativeTime(item.last_message_at)}</TableCell>
                <TableCell>{item.message_count}</TableCell>
                <TableCell><Inline justify="end"><Button variant="ghost" size="icon-sm" aria-label={`Delete text conversation ${item.peer}`} onPress={() => remover.open({ id: item.peer })}><TrashIcon aria-hidden="true" /></Button></Inline></TableCell>
              </TableRow>}
            </For>
          </TableBody>
        </Table>
      </DataTableSection>
      <TextDeleteDialog label="conversation" target={remover.target()} onCancel={() => remover.cancel()} onConfirm={() => remover.confirm()} />
    </>
  );
}
