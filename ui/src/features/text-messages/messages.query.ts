import { adminApi } from '@/adapters';
import type {
  TextCallbackAttempt,
  TextDeliveryState,
  TextMessage,
  TextMessageDetail,
} from '@/adapters/api.g';
import { unwrapProtectedResponse } from '@/features/auth';

export async function listTextMessagePage({
  peer,
  next,
  search,
  signal,
}: {
  peer: string;
  next?: string;
  search?: string;
  signal: AbortSignal;
}): Promise<{ items: TextMessage[]; next: string | null }> {
  return unwrapProtectedResponse(
    await adminApi.listTextConversationMessages(
      peer,
      { next, limit: 50, search: search?.trim() || undefined },
      { signal }
    )
  );
}

export async function getTextMessageDetail({
  peer,
  messageId,
  signal,
}: {
  peer: string;
  messageId: string;
  signal?: AbortSignal;
}): Promise<TextMessageDetail> {
  return unwrapProtectedResponse(
    await adminApi.getTextMessage(peer, messageId, { signal })
  );
}

export async function deleteTextMessage({
  peer,
  messageId,
  signal,
}: {
  peer: string;
  messageId: string;
  signal?: AbortSignal;
}): Promise<void> {
  unwrapProtectedResponse(
    await adminApi.deleteTextMessage(peer, messageId, { signal })
  );
}

export async function transitionTextDelivery({
  messageId,
  state,
  signal,
}: {
  messageId: string;
  state: Exclude<TextDeliveryState, 'accepted'>;
  signal?: AbortSignal;
}): Promise<TextMessageDetail> {
  return unwrapProtectedResponse(
    await adminApi.transitionTextDelivery(messageId, { state }, { signal })
  );
}

export async function retryTextCallback({
  attemptId,
  signal,
}: {
  attemptId: string;
  signal?: AbortSignal;
}): Promise<TextCallbackAttempt> {
  return unwrapProtectedResponse(
    await adminApi.retryTextCallback(attemptId, { signal })
  );
}

export async function downloadTextMedia({
  peer,
  messageId,
  mediaId,
  signal,
}: {
  peer: string;
  messageId: string;
  mediaId: string;
  signal?: AbortSignal;
}): Promise<Blob> {
  return unwrapProtectedResponse(
    await adminApi.getTextMessageMedia(peer, messageId, mediaId, { signal })
  ) as Blob;
}
