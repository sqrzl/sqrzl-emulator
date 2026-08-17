import { state } from '@askrjs/askr';
import { For, Show } from '@askrjs/askr/control';
import { createMutation } from '@askrjs/askr/data';
import { Link } from '@askrjs/askr/router';
import { ChevronDownIcon, ChevronRightIcon, TrashIcon } from '@askrjs/lucide';
import { Badge, Button, Block } from '@askrjs/themes/components';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeaderCell,
  TableRow,
} from '@askrjs/themes/components';
import type { TextMessage } from '../../adapters/api.g';
import { useCursorList } from '../../features/storage/use-cursor-list';
import { useDeleteTarget } from '../../features/storage/use-delete-target';
import {
  textConversationListKey,
  textMessageListKey,
} from '../../features/texts/keys';
import {
  deleteTextMessage,
  listTextMessagePage,
} from '../../features/texts/texts.query';
import { formatRelativeTime, formatTextProvider } from '../../shared/format';
import { textMessagePath } from '../../shared/routes';
import DataTableSection from '../storage/data-table-section';
import TextDeleteDialog from './text-delete-dialog';
import TextMessageDetails from './text-message-details';

export default function TextMessageTable({ peer }: { peer: string }) {
  const [expandedMessageId, setExpandedMessageId] = state<string | null>(null);
  const key = textMessageListKey(peer);
  const list = useCursorList<TextMessage>(
    key,
    'search',
    ({ next, search, signal }) =>
      listTextMessagePage({ peer, next, search, signal })
  );
  const remove = createMutation({
    action: (target: { id: string }, { signal }) =>
      deleteTextMessage({ peer, messageId: target.id, signal }),
    affects: () => [key, textConversationListKey],
    afterSuccess: 'invalidate',
  });
  const remover = useDeleteTarget<{ id: string }>({
    keyOf: (target) => target.id,
    remove: async (target) => {
      await remove.execute(target);
    },
    removeError: 'Text message could not be deleted.',
  });
  const items = list.items();
  const hasItems = items.length > 0;
  return (
    <>
      <DataTableSection
        searchInputId="text-message-search"
        searchLabel="Search messages"
        searchValue={list.search()}
        onSearch={list.setSearch}
        loading={list.pending() && !hasItems}
        errored={Boolean(list.error()) && !hasItems}
        empty={!hasItems}
        emptyTitle={
          list.search()
            ? 'No messages match this search'
            : 'No messages in this conversation'
        }
        emptyDescription={
          list.search()
            ? 'Try another sender, recipient, or body.'
            : 'Simulate an inbound text or send through a provider API.'
        }
        errorTitle="Text messages could not load"
        errorDescription="Retry the admin API call to see this conversation."
        onRetry={() => list.refresh()}
        hasNext={list.hasNext()}
        hasPrevious={list.hasPrevious()}
        onNext={() => list.next()}
        onPrevious={() => list.previous()}
      >
        <Table>
          <TableHead>
            <TableRow>
              <TableHeaderCell>Time</TableHeaderCell>
              <TableHeaderCell>Direction</TableHeaderCell>
              <TableHeaderCell>Body</TableHeaderCell>
              <TableHeaderCell>Provider</TableHeaderCell>
              <TableHeaderCell>Channel</TableHeaderCell>
              <TableHeaderCell>Status</TableHeaderCell>
              <TableHeaderCell>
                <Block direction="row" justify="end">
                  Actions
                </Block>
              </TableHeaderCell>
            </TableRow>
          </TableHead>
          <TableBody>
            <For each={items} by={(item) => item.message_id}>
              {(item) => (
                <>
                  <TableRow key={item.message_id}>
                    <TableCell>
                      <Block direction="row" align="center" gap="xs">
                        <Button
                          variant="ghost"
                          size="icon-sm"
                          aria-label={`${expandedMessageId() === item.message_id ? 'Collapse' : 'Expand'} text message ${item.message_id}`}
                          aria-expanded={
                            expandedMessageId() === item.message_id
                          }
                          aria-controls={`text-message-inline-${item.message_id}`}
                          onPress={() =>
                            setExpandedMessageId(
                              expandedMessageId() === item.message_id
                                ? null
                                : item.message_id
                            )
                          }
                        >
                          {expandedMessageId() === item.message_id ? (
                            <ChevronDownIcon aria-hidden="true" />
                          ) : (
                            <ChevronRightIcon aria-hidden="true" />
                          )}
                        </Button>
                        {formatRelativeTime(item.created_at)}
                      </Block>
                    </TableCell>
                    <TableCell>
                      {item.direction === 'inbound'
                        ? 'Inbound ←'
                        : 'Outbound →'}
                    </TableCell>
                    <TableCell>
                      <Link href={textMessagePath(peer, item.message_id)}>
                        {item.body || '(no body)'}
                      </Link>
                    </TableCell>
                    <TableCell>
                      <Badge variant="secondary">
                        {formatTextProvider(item.provider)}
                      </Badge>
                    </TableCell>
                    <TableCell>
                      <Badge variant="outline">
                        {item.channel.toUpperCase()}
                      </Badge>
                    </TableCell>
                    <TableCell>
                      <Badge
                        variant={
                          item.delivery_state === 'failed'
                            ? 'outline'
                            : 'secondary'
                        }
                      >
                        {item.delivery_state}
                      </Badge>
                    </TableCell>
                    <TableCell>
                      <Block direction="row" justify="end">
                        <Button
                          variant="ghost"
                          size="icon-sm"
                          aria-label={`Delete text message ${item.message_id}`}
                          onPress={() => remover.open({ id: item.message_id })}
                        >
                          <TrashIcon aria-hidden="true" />
                        </Button>
                      </Block>
                    </TableCell>
                  </TableRow>
                  <Show when={expandedMessageId() === item.message_id}>
                    <TableRow key={`${item.message_id}-details`}>
                      <TableCell colSpan={7}>
                        <div
                          id={`text-message-inline-${item.message_id}`}
                          data-sqrzl-slot="inline-message-details"
                        >
                          <TextMessageDetails
                            peer={peer}
                            messageId={item.message_id}
                          />
                        </div>
                      </TableCell>
                    </TableRow>
                  </Show>
                </>
              )}
            </For>
          </TableBody>
        </Table>
      </DataTableSection>
      <TextDeleteDialog
        label="message"
        target={remover.target()}
        onCancel={() => remover.cancel()}
        onConfirm={() => remover.confirm()}
      />
    </>
  );
}
