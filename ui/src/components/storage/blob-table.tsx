import { For } from '@askrjs/askr/control';
import { Link } from '@askrjs/askr/router';
import { Button } from '@askrjs/themes/controls';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeaderCell,
  TableRow,
} from '@askrjs/ui';
import { createMutation } from '@askrjs/askr/data';
import BlobDeleteDialog from './blob-delete-dialog';
import DataTableSection from './data-table-section';
import { useCursorList } from '../../features/storage/use-cursor-list';
import { useDeleteTarget } from '../../features/storage/use-delete-target';
import { blobListKey } from '../../features/storage/keys';
import {
  deleteObject as deleteBlob,
  loadObjectPage as loadBlobPage,
} from '../../features/objects/objects.query';
import type { ObjectInfo as BlobInfo } from '../../adapters/api.g';
import { formatBytes, formatRelativeTime } from '../../shared/format';
import { blobPath } from '../../shared/routes';

function formatBlobSize(size: number): string {
  return `${formatBytes(size)} (${size.toLocaleString()} bytes)`;
}

export default function BlobTable({ bucketName }: { bucketName: string }) {
  const list = useCursorList<BlobInfo>(
    blobListKey(bucketName),
    ({ next, search, signal }) =>
      loadBlobPage({ bucketName, next, search, signal })
  );

  const remove = createMutation({
    action: (id: { blobKey: string }, { signal }) =>
      deleteBlob({ bucketName, objectKey: id.blobKey, signal }),
    affects: () => [blobListKey(bucketName)],
    afterSuccess: 'invalidate',
  });

  const remover = useDeleteTarget<{ blobKey: string }>({
    keyOf: (id) => id.blobKey,
    remove: (id) => remove.execute(id),
    removeError: 'Blob could not be deleted.',
  });

  const blobs = list.items();
  const hasBlobs = blobs.length > 0;
  const hasSearch = list.search().length > 0;

  return (
    <>
      <DataTableSection
        title="Blobs"
        searchInputId="blob-search"
        searchLabel="Search blobs"
        onSearch={list.setSearch}
        loading={list.pending() && !hasBlobs}
        errored={Boolean(list.error()) && !hasBlobs}
        empty={!hasBlobs}
        emptyTitle={
          hasSearch ? 'No blobs match this search' : 'No blobs in this bucket'
        }
        emptyDescription={
          hasSearch
            ? 'Try a different blob key or clear the current search.'
            : 'Upload a file to create the first blob.'
        }
        errorTitle="Blobs could not load"
        errorDescription="Retry the admin API call to see the blob list."
        onRetry={() => list.refresh()}
        hasNext={list.hasNext()}
        hasPrevious={list.hasPrevious()}
        onNext={() => list.next()}
        onPrevious={() => list.previous()}
      >
        <Table>
          <TableHead>
            <TableRow>
              <TableHeaderCell>Blob</TableHeaderCell>
              <TableHeaderCell>Content type</TableHeaderCell>
              <TableHeaderCell>Size</TableHeaderCell>
              <TableHeaderCell>Last modified</TableHeaderCell>
              <TableHeaderCell>Actions</TableHeaderCell>
            </TableRow>
          </TableHead>
          <TableBody>
            <For each={blobs} by={(blob) => blob.key}>
              {(blob) => (
                <TableRow key={blob.key}>
                  <TableCell>
                    <Link href={blobPath(bucketName, blob.key)}>
                      {blob.key}
                    </Link>
                  </TableCell>
                  <TableCell>
                    {blob.content_type ?? 'application/octet-stream'}
                  </TableCell>
                  <TableCell>{formatBlobSize(blob.size)}</TableCell>
                  <TableCell>
                    {formatRelativeTime(blob.last_modified)}
                  </TableCell>
                  <TableCell>
                    <Button
                      variant="secondary"
                      onPress={() => remover.open({ blobKey: blob.key })}
                    >
                      Delete
                    </Button>
                  </TableCell>
                </TableRow>
              )}
            </For>
          </TableBody>
        </Table>
      </DataTableSection>

      <BlobDeleteDialog
        bucketName={bucketName}
        target={remover.target()}
        onCancel={() => remover.cancel()}
        onConfirm={() => remover.confirm()}
      />
    </>
  );
}
