import { For } from '@askrjs/askr/control';
import { Link } from '@askrjs/askr/router';
import { Button } from '@askrjs/themes/controls';
import { DeleteIcon } from '@askrjs/lucide';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeaderCell,
  TableRow,
} from '@askrjs/ui';
import { createMutation } from '@askrjs/askr/data';
import BucketDeleteDialog from './bucket-delete-dialog';
import DataTableSection from './data-table-section';
import { useCursorList } from '../../features/storage/use-cursor-list';
import { useDeleteTarget } from '../../features/storage/use-delete-target';
import { bucketListKey } from '../../features/storage/keys';
import {
  deleteBucketWithContents,
  listBucketPage,
} from '../../features/buckets/buckets.query';
import { countBucketObjects } from '../../features/objects/objects.query';
import { formatRelativeTime } from '../../shared/format';
import { bucketPath } from '../../shared/routes';

type BucketItem = {
  name: string;
  createdAt: string;
  versioningEnabled: boolean;
};

export default function BucketTable() {
  const list = useCursorList<BucketItem>(
    bucketListKey,
    'search',
    ({ next, search, signal }) => listBucketPage({ next, search, signal })
  );

  const remove = createMutation({
    action: (id: { bucketName: string }, { signal }) =>
      deleteBucketWithContents({ bucketName: id.bucketName, signal }),
    affects: () => [bucketListKey],
    afterSuccess: 'invalidate',
  });

  const remover = useDeleteTarget<{ bucketName: string }>({
    keyOf: (id) => id.bucketName,
    precount: (id, signal) =>
      countBucketObjects({ bucketName: id.bucketName, signal }),
    remove: async (id) => {
      await remove.execute(id);
    },
    removeError: 'Bucket could not be deleted.',
    countError: 'Blob count could not be loaded.',
  });

  const buckets = list.items();
  const hasBuckets = buckets.length > 0;
  const hasSearch = list.search().length > 0;

  return (
    <>
      <DataTableSection
        searchInputId="bucket-search"
        searchLabel="Search buckets"
        searchValue={list.search()}
        onSearch={list.setSearch}
        loading={list.pending() && !hasBuckets}
        errored={Boolean(list.error()) && !hasBuckets}
        empty={!hasBuckets}
        emptyTitle={
          hasSearch ? 'No buckets match this search' : 'No buckets yet'
        }
        emptyDescription={
          hasSearch
            ? 'Try a different name or clear the current search.'
            : 'Create a bucket to start using the emulator.'
        }
        errorTitle="Buckets could not load"
        errorDescription="Retry the admin API call to see the bucket list."
        onRetry={() => list.refresh()}
        hasNext={list.hasNext()}
        hasPrevious={list.hasPrevious()}
        onNext={() => list.next()}
        onPrevious={() => list.previous()}
      >
        <Table>
          <TableHead>
            <TableRow>
              <TableHeaderCell>Bucket</TableHeaderCell>
              <TableHeaderCell>Created</TableHeaderCell>
              <TableHeaderCell>Actions</TableHeaderCell>
            </TableRow>
          </TableHead>
          <TableBody>
            <For each={buckets} by={(bucket) => bucket.name}>
              {(bucket) => (
                <TableRow key={bucket.name}>
                  <TableCell>
                    <Link href={bucketPath(bucket.name)}>{bucket.name}</Link>
                  </TableCell>
                  <TableCell>{formatRelativeTime(bucket.createdAt)}</TableCell>
                  <TableCell>
                    <Button
                      variant="secondary"
                      aria-label={`Delete bucket ${bucket.name}`}
                      onPress={() => remover.open({ bucketName: bucket.name })}
                    >
                      <DeleteIcon aria-hidden="true" /> Delete
                    </Button>
                  </TableCell>
                </TableRow>
              )}
            </For>
          </TableBody>
        </Table>
      </DataTableSection>

      <BucketDeleteDialog
        target={remover.target()}
        onCancel={() => remover.cancel()}
        onConfirm={() => remover.confirm()}
      />
    </>
  );
}
