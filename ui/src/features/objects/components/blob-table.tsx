import { For } from '@askrjs/askr/control';
import { createMutation } from '@askrjs/askr/data';
import { Link } from '@askrjs/askr/router';
import { FileIcon, FolderIcon, TrashIcon } from '@askrjs/lucide';
import { Badge, Button, Block } from '@askrjs/themes/components';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeaderCell,
  TableRow,
} from '@askrjs/themes/components';
import { deleteObject as deleteBlob, loadObjectPage } from '../objects.query';
import { type BlobBrowserRow, splitBlobBrowserRows } from '../blob-browser';
import { createCursorList } from '@/shared/cursor-list';
import { createDeleteTarget } from '@/shared/delete-target';
import { blobListKey } from '../keys';
import { formatByteCount, formatRelativeTime } from '@/shared/format';
import { blobPath, bucketFolderPath } from '@/shared/routes';
import BlobDeleteDialog from './blob-delete-dialog';
import DataTableSection from '@/components/collections/data-table-section';

export default function BlobTable({
  bucketName,
  pathPrefix,
}: {
  bucketName: string;
  pathPrefix: string;
}) {
  const list = createCursorList<BlobBrowserRow>(
    `${blobListKey(bucketName)}:path=${pathPrefix}`,
    'search',
    async ({ search, next, signal }) => {
      const page = await loadObjectPage({
        bucketName,
        next,
        search,
        pathPrefix: pathPrefix || undefined,
        signal,
      });

      return {
        items: [
          ...page.folders.map((folder): BlobBrowserRow => ({
            type: 'folder',
            folder,
          })),
          ...page.items.map((blob): BlobBrowserRow => ({ type: 'blob', blob })),
        ],
        next: page.next,
      };
    }
  );

  const remove = createMutation({
    action: (id: { blobKey: string }, { signal }) =>
      deleteBlob({ bucketName, objectKey: id.blobKey, signal }),
    affects: () => [blobListKey(bucketName)],
    afterSuccess: 'invalidate',
  });

  const remover = createDeleteTarget<{ blobKey: string }>({
    keyOf: (id) => id.blobKey,
    remove: (id) => remove.execute(id),
    removeError: 'Blob could not be deleted.',
  });

  const rows = splitBlobBrowserRows(list.items());
  const hasRows = rows.folders.length > 0 || rows.blobs.length > 0;
  const hasSearch = list.search().length > 0;

  return (
    <>
      <DataTableSection
        title="Blobs"
        searchInputId="blob-search"
        searchLabel="Search blobs"
        searchValue={list.search()}
        onSearch={list.setSearch}
        loading={list.pending() && !hasRows}
        errored={Boolean(list.error()) && !hasRows}
        empty={!hasRows}
        emptyTitle={
          hasSearch
            ? 'No folders or blobs match this search'
            : pathPrefix
              ? 'No blobs in this path'
              : 'No blobs in this bucket'
        }
        emptyDescription={
          hasSearch
            ? 'Try a different blob key or clear the current search.'
            : pathPrefix
              ? 'Upload a file to create the first blob in this path.'
              : 'Upload a file to create the first blob.'
        }
        errorTitle="Path contents could not load"
        errorDescription="Retry the admin API call to see folders and blobs."
        onRetry={() => list.refresh()}
        hasNext={list.hasNext()}
        hasPrevious={list.hasPrevious()}
        onNext={() => list.next()}
        onPrevious={() => list.previous()}
        tableWidth="wide"
      >
        <Table>
          <TableHead>
            <TableRow>
              <TableHeaderCell>Name</TableHeaderCell>
              <TableHeaderCell>Type</TableHeaderCell>
              <TableHeaderCell>Content type</TableHeaderCell>
              <TableHeaderCell>Size</TableHeaderCell>
              <TableHeaderCell>Last modified</TableHeaderCell>
              <TableHeaderCell>
                <Block direction="row" justify="end">
                  Actions
                </Block>
              </TableHeaderCell>
            </TableRow>
          </TableHead>
          <TableBody>
            <For each={rows.folders} by={(folder) => folder.prefix}>
              {(folder) => (
                <TableRow key={folder.prefix}>
                  <TableCell>
                    <Block
                      direction="row"
                      gap="xs"
                      align="center"
                      style={{ flexWrap: 'wrap' }}
                    >
                      <FolderIcon aria-hidden="true" />
                      <Link href={bucketFolderPath(bucketName, folder.prefix)}>
                        {folder.name}
                      </Link>
                    </Block>
                  </TableCell>
                  <TableCell>
                    <Badge variant="secondary">Folder</Badge>
                  </TableCell>
                  <TableCell>-</TableCell>
                  <TableCell>-</TableCell>
                  <TableCell>-</TableCell>
                  <TableCell />
                </TableRow>
              )}
            </For>
            <For each={rows.blobs} by={(blob) => blob.key}>
              {(blob) => (
                <TableRow key={blob.key}>
                  <TableCell>
                    <Block
                      direction="row"
                      gap="xs"
                      align="center"
                      style={{ flexWrap: 'wrap' }}
                    >
                      <FileIcon aria-hidden="true" />
                      <Link href={blobPath(bucketName, blob.key, blob.key)}>
                        {blob.key.slice(pathPrefix.length)}
                      </Link>
                    </Block>
                  </TableCell>
                  <TableCell>
                    <Badge variant="outline">Blob</Badge>
                  </TableCell>
                  <TableCell>
                    {blob.content_type ?? 'application/octet-stream'}
                  </TableCell>
                  <TableCell>{formatByteCount(blob.size)}</TableCell>
                  <TableCell>
                    {formatRelativeTime(blob.last_modified)}
                  </TableCell>
                  <TableCell>
                    <Block direction="row" justify="end" align="center">
                      <Button
                        variant="ghost"
                        size="icon-sm"
                        aria-label={`Delete blob ${blob.key}`}
                        onPress={() => remover.open({ blobKey: blob.key })}
                      >
                        <TrashIcon aria-hidden="true" />
                      </Button>
                    </Block>
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
