import { state } from '@askrjs/askr';
import { For, Show } from '@askrjs/askr/control';
import { resource } from '@askrjs/askr/resources';
import { Link } from '@askrjs/askr/router';
import { ArrowLeftIcon, DownloadIcon } from '@askrjs/lucide';
import {
  Button,
  DataTable,
  EmptyState,
  FieldError,
  Block,
  Toolbar,
} from '@askrjs/themes/components';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeaderCell,
  TableRow,
} from '@askrjs/themes/components';
import {
  downloadObjectContent as downloadBlobContent,
  loadObjectMetadata as loadBlobMetadata,
} from '../objects.query';
import type { ObjectMetadata as BlobMetadata } from '@/adapters/api.g';
import { blobParentPath } from '@/features/storage';
import { formatByteCount, formatRelativeTime } from '@/shared/format';

export default function BlobDetails({
  bucketName,
  blobKey,
}: {
  bucketName: string;
  blobKey: string;
}) {
  const [downloadPending, setDownloadPending] = state(false);
  const [downloadError, setDownloadError] = state('');
  const metadata = resource(
    ({ signal }) =>
      loadBlobMetadata({ bucketName, objectKey: blobKey, signal }),
    [bucketName, blobKey]
  );

  async function handleDownload() {
    if (downloadPending()) {
      return;
    }

    setDownloadPending(true);
    setDownloadError('');

    try {
      const downloaded = await downloadBlobContent({
        bucketName,
        objectKey: blobKey,
      });
      const href = window.URL.createObjectURL(downloaded.blob);
      const link = document.createElement('a');
      link.href = href;
      link.download = downloaded.fileName;
      document.body.appendChild(link);
      link.click();
      link.remove();
      window.setTimeout(() => {
        window.URL.revokeObjectURL(href);
      }, 0);
    } catch (caughtError) {
      setDownloadError(
        caughtError instanceof Error
          ? caughtError.message
          : 'Blob download could not start.'
      );
    } finally {
      setDownloadPending(false);
    }
  }

  return (
    <Block direction="column" gap="md">
      <Block
        direction="row"
        data-sqrzl-slot="storage-detail-actions"
        align="center"
        gap="xs"
        style={{ flexWrap: 'wrap' }}
      >
        <Button variant="secondary" asChild>
          <Link href={blobParentPath(bucketName, blobKey)}>
            <ArrowLeftIcon aria-hidden="true" /> Back
          </Link>
        </Button>
        <Button
          onPress={() => void handleDownload()}
          disabled={downloadPending()}
        >
          <DownloadIcon aria-hidden="true" />
          {downloadPending() ? 'Downloading...' : 'Download blob'}
        </Button>
      </Block>

      <Show when={downloadError()}>
        <FieldError role="alert">{downloadError()}</FieldError>
      </Show>

      <Show when={metadata.error && !metadata.value}>
        <EmptyState
          title="Blob details could not load"
          description="Retry the admin API call to see the blob details."
          actions={<Button onPress={() => metadata.refresh()}>Retry</Button>}
        />
      </Show>

      <Show when={metadata.pending && !metadata.value}>
        <p>Loading blob details...</p>
      </Show>

      <Show when={metadata.value}>
        {(blob: BlobMetadata) => {
          const customMetadata: Array<[string, string]> = Object.entries(
            blob.metadata
          );

          return (
            <Block direction="column" gap="md">
              <section aria-labelledby="blob-details-title">
                <Block direction="column" gap="sm">
                  <Toolbar
                    title={<span id="blob-details-title">Details</span>}
                  />
                  <DataTable
                    data-sqrzl-slot="storage-table-scroll"
                    data-sqrzl-table-width="detail"
                  >
                    <Table>
                      <TableBody>
                        <TableRow>
                          <TableHeaderCell>Bucket</TableHeaderCell>
                          <TableCell>{bucketName}</TableCell>
                        </TableRow>
                        <TableRow>
                          <TableHeaderCell>Key</TableHeaderCell>
                          <TableCell>{blobKey}</TableCell>
                        </TableRow>
                        <TableRow>
                          <TableHeaderCell>Size</TableHeaderCell>
                          <TableCell>{formatByteCount(blob.size)}</TableCell>
                        </TableRow>
                        <TableRow>
                          <TableHeaderCell>Content type</TableHeaderCell>
                          <TableCell>
                            {blob.content_type ?? 'application/octet-stream'}
                          </TableCell>
                        </TableRow>
                        <TableRow>
                          <TableHeaderCell>ETag</TableHeaderCell>
                          <TableCell>{blob.etag}</TableCell>
                        </TableRow>
                        <TableRow>
                          <TableHeaderCell>Version ID</TableHeaderCell>
                          <TableCell>
                            {blob.version_id ?? 'unversioned'}
                          </TableCell>
                        </TableRow>
                        <TableRow>
                          <TableHeaderCell>Storage class</TableHeaderCell>
                          <TableCell>{blob.storage_class}</TableCell>
                        </TableRow>
                        <TableRow>
                          <TableHeaderCell>Last modified</TableHeaderCell>
                          <TableCell>
                            {formatRelativeTime(blob.last_modified)}
                          </TableCell>
                        </TableRow>
                      </TableBody>
                    </Table>
                  </DataTable>
                </Block>
              </section>

              <section aria-labelledby="blob-metadata-title">
                <Block direction="column" gap="sm">
                  <Toolbar
                    title={
                      <span id="blob-metadata-title">Custom metadata</span>
                    }
                  />
                  <Show
                    when={customMetadata.length > 0}
                    fallback={<p>No custom metadata recorded.</p>}
                  >
                    <DataTable
                      data-sqrzl-slot="storage-table-scroll"
                      data-sqrzl-table-width="detail"
                    >
                      <Table>
                        <TableHead>
                          <TableRow>
                            <TableHeaderCell>Name</TableHeaderCell>
                            <TableHeaderCell>Value</TableHeaderCell>
                          </TableRow>
                        </TableHead>
                        <TableBody>
                          <For each={customMetadata} by={([name]) => name}>
                            {([name, value]) => (
                              <TableRow key={name}>
                                <TableCell>{name}</TableCell>
                                <TableCell>{value}</TableCell>
                              </TableRow>
                            )}
                          </For>
                        </TableBody>
                      </Table>
                    </DataTable>
                  </Show>
                </Block>
              </section>
            </Block>
          );
        }}
      </Show>
    </Block>
  );
}
