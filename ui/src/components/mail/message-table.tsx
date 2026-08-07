import { For } from '@askrjs/askr/control';
import { Link } from '@askrjs/askr/router';
import { TrashIcon } from '@askrjs/lucide';
import { Badge, Button, Inline } from '@askrjs/themes/components';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeaderCell,
  TableRow,
} from '@askrjs/ui';
import { createMutation } from '@askrjs/askr/data';
import { MessageSummary } from '../../adapters/api.g';
import { useCursorList } from '../../features/storage/use-cursor-list';
import { useDeleteTarget } from '../../features/storage/use-delete-target';
import { mailboxMessagesListKey } from '../../features/mailboxes/keys';
import { deleteMessage, listMessagePage } from '../../features/messages/messages.query';
import { formatRelativeTime } from '../../shared/format';
import { mailMessagePath } from '../../shared/routes';
import DataTableSection from '../storage/data-table-section';
import MessageDeleteDialog from './message-delete-dialog';

type MailAddressList = Array<{ email: string; name?: string | null }>;

function formatAddress(address: MailAddressList[number]): string {
  return address.name ? `${address.name} <${address.email}>` : address.email;
}

function formatAddressList(addresses: MailAddressList): string {
  return addresses.map(formatAddress).join(', ') || 'Unknown';
}

function deliveryStateTone(
  state: MessageSummary['delivery_state']
): 'secondary' | 'outline' {
  return state === 'rejected' || state === 'bounced'
    ? 'outline'
    : 'secondary';
}

export default function MessageTable({ mailboxName }: { mailboxName: string }) {
  const list = useCursorList<MessageSummary>(
    mailboxMessagesListKey(mailboxName),
    'search',
    ({ next, search, signal }) =>
      listMessagePage({
        mailbox: mailboxName,
        next,
        search,
        signal,
      })
  );

  const remove = createMutation({
    action: (
      target: { mailbox: string; messageId: string },
      { signal }
    ) =>
      deleteMessage({
        mailbox: target.mailbox,
        messageId: target.messageId,
        signal,
      }),
    affects: () => [mailboxMessagesListKey(mailboxName)],
    afterSuccess: 'invalidate',
  });

  const remover = useDeleteTarget<{ mailbox: string; messageId: string }>({
    keyOf: (id) => `${id.mailbox}:${id.messageId}`,
    remove: async (id) => {
      await remove.execute(id);
    },
    removeError: 'Message could not be deleted.',
  });

  const messages = list.items();
  const hasMessages = messages.length > 0;
  const hasSearch = list.search().length > 0;

  return (
    <>
      <DataTableSection
        searchInputId="mail-message-search"
        searchLabel="Search messages"
        searchValue={list.search()}
        onSearch={list.setSearch}
        loading={list.pending() && !hasMessages}
        errored={Boolean(list.error()) && !hasMessages}
        empty={!hasMessages}
        emptyTitle={hasSearch ? 'No messages match this search' : 'No messages'}
        emptyDescription={
          hasSearch
            ? 'Try a different subject or sender and retry the search.'
            : 'No captured messages yet for this mailbox.'
        }
        errorTitle="Messages could not load"
        errorDescription="Retry the admin API call to see mailbox messages."
        onRetry={() => list.refresh()}
        hasNext={list.hasNext()}
        hasPrevious={list.hasPrevious()}
        onNext={() => list.next()}
        onPrevious={() => list.previous()}
      >
        <Table>
          <TableHead>
            <TableRow>
              <TableHeaderCell>Received</TableHeaderCell>
              <TableHeaderCell>From</TableHeaderCell>
              <TableHeaderCell>To</TableHeaderCell>
              <TableHeaderCell>Subject</TableHeaderCell>
              <TableHeaderCell>State</TableHeaderCell>
              <TableHeaderCell>
                <Inline justify="end">Actions</Inline>
              </TableHeaderCell>
            </TableRow>
          </TableHead>
          <TableBody>
            <For each={messages} by={(message) => message.message_id}>
              {(message) => (
                <TableRow key={message.message_id}>
                  <TableCell>
                    {formatRelativeTime(message.received_at)}
                  </TableCell>
                  <TableCell>
                    {formatAddressList([message.from])}
                  </TableCell>
                  <TableCell>
                    {formatAddressList(message.to ? message.to : [])}
                  </TableCell>
                  <TableCell>
                    <Link
                      href={mailMessagePath(mailboxName, message.message_id)}
                    >
                      {message.subject}
                    </Link>
                  </TableCell>
                  <TableCell>
                    <Badge variant={deliveryStateTone(message.delivery_state)}>
                      {message.delivery_state}
                    </Badge>
                  </TableCell>
                  <TableCell>
                    <Inline justify="end" align="center">
                      <Button
                        variant="ghost"
                        size="icon-sm"
                        aria-label={`Delete message ${message.message_id}`}
                        onPress={() =>
                          remover.open({
                            mailbox: mailboxName,
                            messageId: message.message_id,
                          })
                        }
                      >
                        <TrashIcon aria-hidden="true" />
                      </Button>
                    </Inline>
                  </TableCell>
                </TableRow>
              )}
            </For>
          </TableBody>
        </Table>
      </DataTableSection>

      <MessageDeleteDialog
        target={remover.target()}
        onCancel={() => remover.cancel()}
        onConfirm={() => remover.confirm()}
      />
    </>
  );
}
