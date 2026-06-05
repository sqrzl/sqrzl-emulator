import { For } from '@askrjs/askr/control';
import { createMutation } from '@askrjs/askr/data';
import { Link } from '@askrjs/askr/router';
import { FolderIcon } from '@askrjs/lucide';
import { Button } from '@askrjs/themes/controls';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeaderCell,
  TableRow,
} from '@askrjs/ui';
import type { ObjectInfo as BlobInfo } from '../../adapters/api.g';
import {
  deleteObject as deleteBlob,
  loadAllObjectPages,
} from '../../features/objects/objects.query';
import { useCursorList } from '../../features/storage/use-cursor-list';
import { useDeleteTarget } from '../../features/storage/use-delete-target';
import { blobListKey } from '../../features/storage/keys';
import { formatBytes, formatRelativeTime } from '../../shared/format';
import { blobPath, bucketFolderPath } from '../../shared/routes';
import BlobDeleteDialog from './blob-delete-dialog';
import DataTableSection from './data-table-section';

type FolderRow = {
  name: string;
  prefix: string;
};

function formatBlobSize(size: number): string {
  return `${formatBytes(size)} (${size.toLocaleString()} bytes)`;
}

function collectRows(items: BlobInfo[], pathPrefix: string): {
  folders: FolderRow[];
  blobs: BlobInfo[];
} {
  const folderMap = new Map<string, FolderRow>();
  const blobs: BlobInfo[] = [];

  for (const blob of items) {
    if (!blob.key.startsWith(pathPrefix)) {
      continue;
    }

    const relativeKey = blob.key.slice(pathPrefix.length);
    if (!relativeKey) {
      continue;
    }

    const slashIndex = relativeKey.indexOf('/');
    if (slashIndex >= 0) {
      const folderName = `${relativeKey.slice(0, slashIndex + 1)}`;
      const nextPrefix = `${pathPrefix}${folderName}`;
      if (!folderMap.has(folderName)) {
        folderMap.set(folderName, {
          name: folderName,
          prefix: nextPrefix,
        });
      }
      continue;
    }

    blobs.push(blob);
  }

  const folders = Array.from(folderMap.values()).sort((left, right) =>
    left.name.localeCompare(right.name)
  );

  blobs.sort((left, right) => left.key.localeCompare(right.key));
  return { folders, blobs };
}

export default function BlobTable({
  bucketName,
  pathPrefix,
}: {
  bucketName: string;
  pathPrefix: string;
}) {
  const list = useCursorList<BlobInfo>(
    `${blobListKey(bucketName)}:path=${pathPrefix}`,
    'search',
    async ({ search, signal }) => {
      const all = await loadAllObjectPages({
        bucketName,
        search: pathPrefix || undefined,
        signal,
      });

      const query = search?.trim().toLowerCase() ?? '';
      const filtered = query
        ? all.filter((blob) =>
            blob.key.slice(pathPrefix.length).toLowerCase().includes(query)
          )
        : all;

      return {
        items: filtered,
        next: null,
      };
    }
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

  const rows = collectRows(list.items(), pathPrefix);
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
      >
        <Table>
          <TableHead>
            <TableRow>
              <TableHeaderCell>Name</TableHeaderCell>
              <TableHeaderCell>Type</TableHeaderCell>
              <TableHeaderCell>Content type</TableHeaderCell>
              <TableHeaderCell>Size</TableHeaderCell>
              <TableHeaderCell>Last modified</TableHeaderCell>
              <TableHeaderCell>Actions</TableHeaderCell>
            </TableRow>
          </TableHead>
          <TableBody>
            <For each={rows.folders} by={(folder) => folder.prefix}>
              {(folder) => (
                <TableRow key={folder.prefix}>
                  <TableCell>
                    <Link href={bucketFolderPath(bucketName, folder.prefix)}>
                      {folder.name}
                    </Link>
                  </TableCell>
                  <TableCell>Folder</TableCell>
                  <TableCell>-</TableCell>
                  <TableCell>-</TableCell>
                  <TableCell>-</TableCell>
                  <TableCell>
                    <Button variant="secondary" asChild>
                      <Link href={bucketFolderPath(bucketName, folder.prefix)}>
                        <FolderIcon aria-hidden="true" /> Open
                      </Link>
                    </Button>
                  </TableCell>
                </TableRow>
              )}
            </For>
            <For each={rows.blobs} by={(blob) => blob.key}>
              {(blob) => (
                <TableRow key={blob.key}>
                  <TableCell>
                    <Link href={blobPath(bucketName, blob.key)}>
                      {blob.key.slice(pathPrefix.length)}
                    </Link>
                  </TableCell>
                  <TableCell>Blob</TableCell>
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
