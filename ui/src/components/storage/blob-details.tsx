import { state } from '@askrjs/askr';
import { For, Show } from '@askrjs/askr/control';
import { resource } from '@askrjs/askr/resources';
import { Link } from '@askrjs/askr/router';
import { Button, ButtonGroup, FieldError } from '@askrjs/themes/controls';
import { EmptyState } from '@askrjs/themes/feedback';
import { Stack } from '@askrjs/themes/layouts';
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from '@askrjs/themes/surfaces';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeaderCell,
  TableRow,
} from '@askrjs/ui';
import {
  downloadObjectContent as downloadBlobContent,
  loadObjectMetadata as loadBlobMetadata,
} from '../../features/objects/objects.query';
import type { ObjectMetadata as BlobMetadata } from '../../adapters/api.g';
import { formatBytes, formatRelativeTime } from '../../shared/format';
import { bucketPath } from '../../shared/routes';

function formatBlobSize(size: number): string {
  return `${formatBytes(size)} (${size.toLocaleString()} bytes)`;
}

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

  if (metadata.error && !metadata.value) {
    return (
      <EmptyState
        title="Blob details could not load"
        description="Retry the admin API call to see the blob details."
        actions={<Button onPress={() => metadata.refresh()}>Retry</Button>}
      />
    );
  }

  const customMetadata: Array<[string, string]> = metadata.value
    ? Object.entries(metadata.value.metadata)
    : [];

  return (
    <Stack gap="4">
      <ButtonGroup>
        <Button variant="secondary" asChild>
          <Link href={bucketPath(bucketName)}>Back to bucket</Link>
        </Button>
        <Button
          onPress={() => void handleDownload()}
          disabled={downloadPending()}
        >
          {downloadPending() ? 'Downloading...' : 'Download blob'}
        </Button>
      </ButtonGroup>

      <Show when={downloadError()}>
        <FieldError role="alert">{downloadError()}</FieldError>
      </Show>

      <Show when={metadata.pending && !metadata.value}>
        <p>Loading blob details...</p>
      </Show>

      <Show when={metadata.value}>
        {(blob: BlobMetadata) => (
          <Stack gap="4">
            <Card>
              <CardHeader>
                <CardTitle>Details</CardTitle>
              </CardHeader>
              <CardContent>
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
                      <TableCell>{formatBlobSize(blob.size)}</TableCell>
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
                      <TableCell>{blob.version_id ?? 'unversioned'}</TableCell>
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
              </CardContent>
            </Card>

            <Card>
              <CardHeader>
                <CardTitle>Custom metadata</CardTitle>
              </CardHeader>
              <CardContent>
                <Show
                  when={customMetadata.length > 0}
                  fallback={<p>No custom metadata recorded.</p>}
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
                </Show>
              </CardContent>
            </Card>
          </Stack>
        )}
      </Show>
    </Stack>
  );
}
