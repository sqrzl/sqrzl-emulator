import { For } from '@askrjs/askr/control';
import { Link } from '@askrjs/askr/router';
import { TrashIcon } from '@askrjs/lucide';
import { Button, Inline } from '@askrjs/themes/components';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeaderCell,
  TableRow,
} from '@askrjs/ui';
import { createMutation } from '@askrjs/askr/data';
import { MailboxInfo } from '../../adapters/api.g';
import { useCursorList } from '../../features/storage/use-cursor-list';
import { useDeleteTarget } from '../../features/storage/use-delete-target';
import { mailboxListKey } from '../../features/mailboxes/keys';
import { deleteMailbox, listMailboxPage } from '../../features/mailboxes/mailboxes.query';
import { formatRelativeTime } from '../../shared/format';
import { mailboxPath } from '../../shared/routes';
import DataTableSection from '../storage/data-table-section';
import MailboxDeleteDialog from './mailbox-delete-dialog';

export default function MailboxTable() {
  const list = useCursorList<MailboxInfo>(
    mailboxListKey,
    'search',
    ({ next, search, signal }) => listMailboxPage({ next, search, signal })
  );

  const remove = createMutation({
    action: (target: { mailbox: string }, { signal }) =>
      deleteMailbox({ mailbox: target.mailbox, signal }),
    affects: () => [mailboxListKey],
    afterSuccess: 'invalidate',
  });

  const remover = useDeleteTarget<{ mailbox: string }>({
    keyOf: (target) => target.mailbox,
    remove: async (target) => {
      await remove.execute(target);
    },
    removeError: 'Mailbox could not be deleted.',
  });

  const mailboxes = list.items();
  const hasMailboxes = mailboxes.length > 0;
  const hasSearch = list.search().length > 0;

  return (
    <>
      <DataTableSection
        searchInputId="mailbox-search"
        searchLabel="Search mailboxes"
        searchValue={list.search()}
        onSearch={list.setSearch}
        loading={list.pending() && !hasMailboxes}
        errored={Boolean(list.error()) && !hasMailboxes}
        empty={!hasMailboxes}
        emptyTitle={
          hasSearch ? 'No mailboxes match this search' : 'No mailboxes yet'
        }
        emptyDescription={
          hasSearch
            ? 'Try a different address or clear the current search.'
            : 'Send mail to the emulator to start collecting captured messages.'
        }
        errorTitle="Mailboxes could not load"
        errorDescription="Retry the admin API call to see the mailbox list."
        onRetry={() => list.refresh()}
        hasNext={list.hasNext()}
        hasPrevious={list.hasPrevious()}
        onNext={() => list.next()}
        onPrevious={() => list.previous()}
      >
        <Table>
          <TableHead>
            <TableRow>
              <TableHeaderCell>Mailbox</TableHeaderCell>
              <TableHeaderCell>Messages</TableHeaderCell>
              <TableHeaderCell>Last received</TableHeaderCell>
              <TableHeaderCell>
                <Inline justify="end">Actions</Inline>
              </TableHeaderCell>
            </TableRow>
          </TableHead>
          <TableBody>
            <For each={mailboxes} by={(mailbox) => mailbox.address}>
              {(mailbox) => (
                <TableRow key={mailbox.address}>
                  <TableCell>
                    <Link href={mailboxPath(mailbox.address)}>
                      {mailbox.address}
                    </Link>
                  </TableCell>
                  <TableCell>{mailbox.message_count}</TableCell>
                  <TableCell>
                    {mailbox.last_received_at
                      ? formatRelativeTime(mailbox.last_received_at)
                      : 'Never'}
                  </TableCell>
                  <TableCell>
                    <Inline justify="end" align="center">
                      <Button
                        variant="ghost"
                        size="icon-sm"
                        aria-label={`Delete mailbox ${mailbox.address}`}
                        onPress={() => remover.open({ mailbox: mailbox.address })}
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

      <MailboxDeleteDialog
        target={remover.target()}
        onCancel={() => remover.cancel()}
        onConfirm={() => remover.confirm()}
      />
    </>
  );
}
