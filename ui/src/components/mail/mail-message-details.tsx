import { For, Show } from '@askrjs/askr/control';
import { resource } from '@askrjs/askr/resources';
import { state } from '@askrjs/askr';
import { Link } from '@askrjs/askr/router';
import { ArrowLeftIcon, DownloadIcon } from '@askrjs/lucide';
import {
  Button,
  DataTable,
  EmptyState,
  FieldError,
  Inline,
  Spinner,
  Stack,
  Toolbar,
} from '@askrjs/themes/components';
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeaderCell,
  TableRow,
} from '@askrjs/ui';
import {
  MailAddress,
  MessageAttachmentSummary,
  MessageDetail,
} from '../../adapters/api.g';
import { formatByteCount, formatRelativeTime } from '../../shared/format';
import { mailboxPath } from '../../shared/routes';
import {
  downloadMessageAttachment,
  downloadMessageContent,
  getMessageDetail,
} from '../../features/messages/messages.query';

export default function MailMessageDetails({
  mailboxName,
  messageId,
}: {
  mailboxName: string;
  messageId: string;
}) {
  const detail = resource(
    ({ signal }) =>
      getMessageDetail({
        mailbox: mailboxName,
        messageId,
        signal,
      }),
    [mailboxName, messageId]
  );

  const [contentDownloadPending, setContentDownloadPending] = state(false);
  const [attachmentDownloadPendingId, setAttachmentDownloadPendingId] = state<
    string | null
  >(null);
  const [downloadError, setDownloadError] = state('');

  function formatMailAddress({ email, name }: MailAddress): string {
    return name ? `${name} <${email}>` : email;
  }

  function formatAddressList(addresses: MailAddress[]): string {
    return addresses.map(formatMailAddress).join(', ') || 'None';
  }

  async function startDownload({
    filename,
    resolve,
  }: {
    filename: string;
    resolve: () => Promise<Blob>;
  }) {
    const blob = await resolve();
    const href = window.URL.createObjectURL(blob);
    const link = document.createElement('a');
    link.href = href;
    link.download = filename;
    document.body.appendChild(link);
    link.click();
    link.remove();
    window.setTimeout(() => {
      window.URL.revokeObjectURL(href);
    }, 0);
  }

  async function downloadContent() {
    if (contentDownloadPending()) {
      return;
    }

    setContentDownloadPending(true);
    setDownloadError('');
    try {
      await startDownload({
        filename: `${messageId}.eml`,
        resolve: () =>
          downloadMessageContent({
            mailbox: mailboxName,
            messageId,
          }),
      });
    } catch (error) {
      setDownloadError(
        error instanceof Error
          ? error.message
          : 'The raw message content could not be downloaded.'
      );
    } finally {
      setContentDownloadPending(false);
    }
  }

  async function downloadAttachment(attachment: MessageAttachmentSummary) {
    if (attachmentDownloadPendingId()) {
      return;
    }

    setAttachmentDownloadPendingId(attachment.filename);
    setDownloadError('');
    try {
      await startDownload({
        filename: attachment.filename,
        resolve: () =>
          downloadMessageAttachment({
            mailbox: mailboxName,
            messageId,
            filename: attachment.filename,
          }),
      });
    } catch (error) {
      setDownloadError(
        error instanceof Error
          ? error.message
          : `The attachment ${attachment.filename} could not be downloaded.`
      );
    } finally {
      setAttachmentDownloadPendingId(null);
    }
  }

  return (
    <Stack gap="4">
      <Inline
        data-sqrzl-slot="mail-message-actions"
        align="center"
        gap="2"
        wrap
      >
        <Button variant="secondary" asChild>
          <Link href={mailboxPath(mailboxName)}>
            <ArrowLeftIcon aria-hidden="true" /> Back to mailbox
          </Link>
        </Button>
        <Button
          onPress={() => void downloadContent()}
          disabled={contentDownloadPending()}
        >
          <DownloadIcon aria-hidden="true" />
          {contentDownloadPending() ? 'Downloading...' : 'Download raw content'}
        </Button>
      </Inline>

      <Show when={downloadError()}>
        <FieldError role="alert">{downloadError()}</FieldError>
      </Show>

      <Show when={detail.error && !detail.value}>
        <EmptyState
          title="Message could not load"
          description="Retry the admin API call to see message details."
          actions={<Button onPress={() => detail.refresh()}>Retry</Button>}
        />
      </Show>

      <Show when={detail.pending && !detail.value}>
        <Inline justify="center" align="center">
          <Spinner />
        </Inline>
      </Show>

      <Show when={detail.value}>
        {(message: MessageDetail) => (
          <Stack gap="4">
            <section aria-labelledby="mail-message-summary-title">
              <Stack gap="3">
                <Toolbar
                  title={<span id="mail-message-summary-title">Summary</span>}
                />
                <DataTable
                  data-sqrzl-slot="storage-table-scroll"
                  data-sqrzl-table-width="detail"
                >
                  <Table>
                    <TableBody>
                      <TableRow>
                        <TableHeaderCell>Mailbox</TableHeaderCell>
                        <TableCell>{message.mailbox}</TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>Message ID</TableHeaderCell>
                        <TableCell>{message.message_id}</TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>Received</TableHeaderCell>
                        <TableCell>{formatRelativeTime(message.received_at)}</TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>From</TableHeaderCell>
                        <TableCell>
                          {formatAddressList([message.from])}
                        </TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>To</TableHeaderCell>
                        <TableCell>{formatAddressList(message.to)}</TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>CC</TableHeaderCell>
                        <TableCell>{formatAddressList(message.cc)}</TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>BCC</TableHeaderCell>
                        <TableCell>{formatAddressList(message.bcc)}</TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>Subject</TableHeaderCell>
                        <TableCell>{message.subject}</TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>Source protocol</TableHeaderCell>
                        <TableCell>{message.source_protocol}</TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>Delivery state</TableHeaderCell>
                        <TableCell>{message.delivery_state}</TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>Delivery detail</TableHeaderCell>
                        <TableCell>{message.delivery_detail ?? '—'}</TableCell>
                      </TableRow>
                      <TableRow>
                        <TableHeaderCell>Thread ID</TableHeaderCell>
                        <TableCell>{message.thread_id ?? '—'}</TableCell>
                      </TableRow>
                    </TableBody>
                  </Table>
                </DataTable>
              </Stack>
            </section>

            <section aria-labelledby="mail-message-headers-title">
              <Stack gap="3">
                <Toolbar
                  title={<span id="mail-message-headers-title">Headers</span>}
                />
                <Show
                  when={Object.keys(message.headers).length > 0}
                  fallback={<p>No headers were captured for this message.</p>}
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
                        <For each={Object.entries(message.headers)} by={([name]) => name}>
                          {([name, value]) => (
                            <TableRow key={name}>
                              <TableHeaderCell>{name}</TableHeaderCell>
                              <TableCell>{value}</TableCell>
                            </TableRow>
                          )}
                        </For>
                      </TableBody>
                    </Table>
                  </DataTable>
                </Show>
              </Stack>
            </section>

            <section aria-labelledby="mail-message-body-title">
              <Stack gap="3">
                <Toolbar title={<span id="mail-message-body-title">Body</span>} />
                <Show when={Boolean(message.body_text)}>
                  <pre>{message.body_text ?? ''}</pre>
                </Show>
                <Show when={Boolean(message.body_html)}>
                  <pre>{message.body_html ?? ''}</pre>
                </Show>
              </Stack>
            </section>

            <section aria-labelledby="mail-message-attachments-title">
              <Stack gap="3">
                <Toolbar
                  title={
                    <span id="mail-message-attachments-title">Attachments</span>
                  }
                />
                <Show
                  when={message.attachments.length > 0}
                  fallback={<p>No attachments on this message.</p>}
                >
                  <DataTable
                    data-sqrzl-slot="storage-table-scroll"
                    data-sqrzl-table-width="detail"
                  >
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
                        <For each={message.attachments} by={(attachment) => attachment.filename}>
                          {(attachment) => (
                            <TableRow key={attachment.filename}>
                              <TableCell>{attachment.filename}</TableCell>
                              <TableCell>{attachment.content_type}</TableCell>
                              <TableCell>{formatByteCount(attachment.size)}</TableCell>
                              <TableCell>
                                <Button
                                  variant="secondary"
                                  disabled={attachmentDownloadPendingId() === attachment.filename}
                                  onPress={() => void downloadAttachment(attachment)}
                                >
                                  <DownloadIcon aria-hidden="true" />
                                  {attachmentDownloadPendingId() === attachment.filename
                                    ? 'Downloading...'
                                    : 'Download'}
                                </Button>
                              </TableCell>
                            </TableRow>
                          )}
                        </For>
                      </TableBody>
                    </Table>
                  </DataTable>
                </Show>
              </Stack>
            </section>
          </Stack>
        )}
      </Show>
    </Stack>
  );
}
