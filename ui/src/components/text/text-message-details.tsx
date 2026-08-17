import { state } from '@askrjs/askr';
import { For, Show } from '@askrjs/askr/control';
import { resource } from '@askrjs/askr/resources';
import { DownloadIcon, ExternalLinkIcon, RefreshCwIcon } from '@askrjs/lucide';
import {
  Badge,
  Block,
  Button,
  DataTable,
  EmptyState,
  FieldError,
  Spinner,
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
import type { TextMedia, TextMessageDetail } from '../../adapters/api.g';
import {
  downloadTextMedia,
  getTextMessageDetail,
  retryTextCallback,
  transitionTextDelivery,
} from '../../features/texts/texts.query';
import {
  formatByteCount,
  formatRelativeTime,
  formatTextProvider,
} from '../../shared/format';

export default function TextMessageDetails({
  peer,
  messageId,
}: {
  peer: string;
  messageId: string;
}) {
  const detail = resource(
    ({ signal }) => getTextMessageDetail({ peer, messageId, signal }),
    [peer, messageId]
  );
  const [action, setAction] = state('');
  const [error, setError] = state('');

  async function transition(state: 'delivered' | 'failed'): Promise<void> {
    if (action()) return;
    setAction(state);
    setError('');
    try {
      await transitionTextDelivery({ messageId, state });
      detail.refresh();
    } catch (caught) {
      setError(
        caught instanceof Error
          ? caught.message
          : 'Delivery state could not be changed.'
      );
    } finally {
      setAction('');
    }
  }

  async function retry(attemptId: string): Promise<void> {
    if (action()) return;
    setAction(attemptId);
    setError('');
    try {
      await retryTextCallback({ attemptId });
      detail.refresh();
    } catch (caught) {
      setError(
        caught instanceof Error ? caught.message : 'Callback retry failed.'
      );
    } finally {
      setAction('');
    }
  }

  async function download(media: TextMedia): Promise<void> {
    if (action()) return;
    setAction(media.media_id);
    setError('');
    try {
      const blob = await downloadTextMedia({
        peer,
        messageId,
        mediaId: media.media_id,
      });
      const href = window.URL.createObjectURL(blob);
      const link = document.createElement('a');
      link.href = href;
      link.download = media.filename;
      document.body.appendChild(link);
      link.click();
      link.remove();
      window.setTimeout(() => window.URL.revokeObjectURL(href), 0);
    } catch (caught) {
      setError(
        caught instanceof Error
          ? caught.message
          : 'Media could not be downloaded.'
      );
    } finally {
      setAction('');
    }
  }

  return (
    <Block direction="column" gap="md">
      <Show when={error()}>
        <FieldError role="alert">{error()}</FieldError>
      </Show>
      <Show when={detail.error && !detail.value}>
        <EmptyState
          title="Text message could not load"
          description="Retry the admin API call to inspect this message."
          actions={<Button onPress={() => detail.refresh()}>Retry</Button>}
        />
      </Show>
      <Show when={detail.pending && !detail.value}>
        <Block direction="row" justify="center">
          <Spinner />
        </Block>
      </Show>
      <Show when={detail.value}>
        {(message: TextMessageDetail) => (
          <Block direction="column" gap="md">
            <Show
              when={
                message.direction === 'outbound' &&
                message.delivery_state === 'accepted'
              }
            >
              <Block
                direction="row"
                gap="xs"
                style={{ flexWrap: 'wrap' }}
                data-sqrzl-slot="storage-detail-actions"
              >
                <Button
                  disabled={Boolean(action())}
                  onPress={() => void transition('delivered')}
                >
                  Mark delivered
                </Button>
                <Button
                  variant="destructive"
                  disabled={Boolean(action())}
                  onPress={() => void transition('failed')}
                >
                  Mark failed
                </Button>
              </Block>
            </Show>
            <section aria-labelledby="text-summary-title">
              <Block direction="column" gap="sm">
                <Toolbar title={<span id="text-summary-title">Summary</span>} />
                <DataTable
                  data-sqrzl-slot="storage-table-scroll"
                  data-sqrzl-table-width="detail"
                >
                  <Table>
                    <TableBody>
                      <TableRow>
                        <TableHeaderCell>Message ID</TableHeaderCell>
                        <TableCell>{message.message_id}</TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>Provider message ID</TableHeaderCell>
                        <TableCell>{message.provider_message_id}</TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>Batch ID</TableHeaderCell>
                        <TableCell>{message.batch_id ?? '—'}</TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>Provider</TableHeaderCell>
                        <TableCell>
                          <Badge variant="secondary">
                            {formatTextProvider(message.provider)}
                          </Badge>
                        </TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>Direction</TableHeaderCell>
                        <TableCell>{message.direction}</TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>Channel</TableHeaderCell>
                        <TableCell>{message.channel.toUpperCase()}</TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>From</TableHeaderCell>
                        <TableCell>{message.from}</TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>To</TableHeaderCell>
                        <TableCell>{message.to}</TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>Created</TableHeaderCell>
                        <TableCell>
                          {formatRelativeTime(message.created_at)}
                        </TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>Delivery state</TableHeaderCell>
                        <TableCell>
                          <Badge
                            variant={
                              message.delivery_state === 'failed'
                                ? 'outline'
                                : 'secondary'
                            }
                          >
                            {message.delivery_state}
                          </Badge>
                        </TableCell>
                      </TableRow>
                    </TableBody>
                  </Table>
                </DataTable>
              </Block>
            </section>

            <section aria-labelledby="text-body-title">
              <Block direction="column" gap="sm">
                <Toolbar title={<span id="text-body-title">Body</span>} />
                <pre data-sqrzl-slot="text-payload">
                  {message.body || '(empty body)'}
                </pre>
              </Block>
            </section>

            <section aria-labelledby="text-media-title">
              <Block direction="column" gap="sm">
                <Toolbar title={<span id="text-media-title">Media</span>} />
                <Show
                  when={message.media.length > 0}
                  fallback={<p>No media on this message.</p>}
                >
                  <DataTable data-sqrzl-slot="storage-table-scroll">
                    <Table>
                      <TableHead>
                        <TableRow>
                          <TableHeaderCell>Filename</TableHeaderCell>
                          <TableHeaderCell>Content type</TableHeaderCell>
                          <TableHeaderCell>Size</TableHeaderCell>
                          <TableHeaderCell>Actions</TableHeaderCell>
                        </TableRow>
                      </TableHead>
                      <TableBody>
                        <For
                          each={message.media}
                          by={(media) => media.media_id}
                        >
                          {(media) => (
                            <TableRow key={media.media_id}>
                              <TableCell>{media.filename}</TableCell>
                              <TableCell>{media.content_type}</TableCell>
                              <TableCell>
                                {media.size == null
                                  ? 'External'
                                  : formatByteCount(media.size)}
                              </TableCell>
                              <TableCell>
                                {media.external_url ? (
                                  <Button variant="secondary" asChild>
                                    <a
                                      href={media.external_url}
                                      target="_blank"
                                      rel="noreferrer"
                                    >
                                      <ExternalLinkIcon aria-hidden="true" />{' '}
                                      Open external media
                                    </a>
                                  </Button>
                                ) : (
                                  <Button
                                    variant="secondary"
                                    disabled={Boolean(action())}
                                    onPress={() => void download(media)}
                                  >
                                    <DownloadIcon aria-hidden="true" />{' '}
                                    {action() === media.media_id
                                      ? 'Downloading...'
                                      : 'Download'}
                                  </Button>
                                )}
                              </TableCell>
                            </TableRow>
                          )}
                        </For>
                      </TableBody>
                    </Table>
                  </DataTable>
                </Show>
              </Block>
            </section>

            <section aria-labelledby="text-callback-title">
              <Block direction="column" gap="sm">
                <Toolbar
                  title={
                    <span id="text-callback-title">Callback attempts</span>
                  }
                />
                <Show
                  when={message.callback_attempts.length > 0}
                  fallback={
                    <p>
                      No callback was attempted. Configure a destination or
                      status callback first.
                    </p>
                  }
                >
                  <DataTable
                    data-sqrzl-slot="storage-table-scroll"
                    data-sqrzl-table-width="wide"
                  >
                    <Table>
                      <TableHead>
                        <TableRow>
                          <TableHeaderCell>Attempt</TableHeaderCell>
                          <TableHeaderCell>Kind</TableHeaderCell>
                          <TableHeaderCell>State</TableHeaderCell>
                          <TableHeaderCell>HTTP</TableHeaderCell>
                          <TableHeaderCell>Request / response</TableHeaderCell>
                          <TableHeaderCell>Actions</TableHeaderCell>
                        </TableRow>
                      </TableHead>
                      <TableBody>
                        <For
                          each={message.callback_attempts}
                          by={(attempt) => attempt.attempt_id}
                        >
                          {(attempt) => (
                            <TableRow key={attempt.attempt_id}>
                              <TableCell>
                                {attempt.attempt_id}
                                <br />
                                {formatRelativeTime(attempt.attempted_at)}
                                {attempt.retry_of ? (
                                  <>
                                    <br />
                                    Retry of {attempt.retry_of}
                                  </>
                                ) : null}
                              </TableCell>
                              <TableCell>{attempt.kind}</TableCell>
                              <TableCell>
                                <Badge
                                  variant={
                                    attempt.state === 'failed'
                                      ? 'outline'
                                      : 'secondary'
                                  }
                                >
                                  {attempt.state}
                                </Badge>
                              </TableCell>
                              <TableCell>
                                {attempt.response_status ?? 'No response'}
                                {attempt.error ? (
                                  <>
                                    <br />
                                    {attempt.error}
                                  </>
                                ) : null}
                              </TableCell>
                              <TableCell>
                                <details>
                                  <summary>Inspect payload</summary>
                                  <p>{attempt.url}</p>
                                  <pre data-sqrzl-slot="text-payload">
                                    {attempt.request_body}
                                  </pre>
                                  <pre data-sqrzl-slot="text-payload">
                                    {attempt.response_body ??
                                      '(no response body)'}
                                  </pre>
                                </details>
                              </TableCell>
                              <TableCell>
                                <Button
                                  variant="secondary"
                                  disabled={Boolean(action())}
                                  onPress={() => void retry(attempt.attempt_id)}
                                >
                                  <RefreshCwIcon aria-hidden="true" />{' '}
                                  {action() === attempt.attempt_id
                                    ? 'Retrying...'
                                    : 'Retry'}
                                </Button>
                              </TableCell>
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
        )}
      </Show>
    </Block>
  );
}
