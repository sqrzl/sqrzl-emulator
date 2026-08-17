import { adminApi } from '@/adapters';
import type { MessageDetail, MessageSummary } from '@/adapters/api.g';
import { unwrapProtectedResponse } from '@/features/auth';

export type MessagePage = {
  items: MessageSummary[];
  next: string | null;
};

export async function listMessagePage({
  mailbox,
  next,
  search,
  signal,
}: {
  mailbox: string;
  next?: string;
  search?: string;
  signal: AbortSignal;
}): Promise<MessagePage> {
  const result = unwrapProtectedResponse(
    await adminApi.listMailboxMessages(
      mailbox,
      { next, limit: 50, search: search?.trim() || undefined },
      { signal }
    )
  );

  return {
    items: result.items,
    next: result.next,
  };
}

export async function getMessageDetail({
  mailbox,
  messageId,
  signal,
}: {
  mailbox: string;
  messageId: string;
  signal?: AbortSignal;
}): Promise<MessageDetail> {
  return unwrapProtectedResponse(
    await adminApi.getMailboxMessage(mailbox, messageId, { signal })
  );
}

export async function deleteMessage({
  mailbox,
  messageId,
  signal,
}: {
  mailbox: string;
  messageId: string;
  signal?: AbortSignal;
}): Promise<void> {
  unwrapProtectedResponse(
    await adminApi.deleteMailboxMessage(mailbox, messageId, { signal })
  );
}

export async function downloadMessageContent({
  mailbox,
  messageId,
  signal,
}: {
  mailbox: string;
  messageId: string;
  signal?: AbortSignal;
}): Promise<Blob> {
  const response = await adminApi.getMailboxMessageContent(mailbox, messageId, {
    signal,
  });
  const data = unwrapProtectedResponse(response);
  return data as Blob;
}
export async function downloadMessageAttachment({
  mailbox,
  messageId,
  filename,
  signal,
}: {
  mailbox: string;
  messageId: string;
  filename: string;
  signal?: AbortSignal;
}): Promise<Blob> {
  const response = await adminApi.getMailboxMessageAttachment(
    mailbox,
    messageId,
    filename,
    { signal }
  );
  const data = unwrapProtectedResponse(response);
  return data as Blob;
}
