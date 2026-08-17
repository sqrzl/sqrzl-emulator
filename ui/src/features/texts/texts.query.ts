import { adminApi } from '../../adapters';
import type {
  InboundTextSimulationRequest,
  TextCallbackAttempt,
  TextConversation,
  TextDeliveryState,
  TextMessage,
  TextMessageDetail,
  TextProvider,
} from '../../adapters/api.g';
import { unwrapProtectedResponse } from '../auth/admin-session';

export async function listTextConversationPage({
  next,
  search,
  signal,
}: {
  next?: string;
  search?: string;
  signal: AbortSignal;
}): Promise<{ items: TextConversation[]; next: string | null }> {
  return unwrapProtectedResponse(
    await adminApi.listTextConversations(
      { next, limit: 50, search: search?.trim() || undefined },
      { signal }
    )
  );
}

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

export async function deleteTextConversation({
  peer,
  signal,
}: {
  peer: string;
  signal?: AbortSignal;
}): Promise<void> {
  unwrapProtectedResponse(
    await adminApi.deleteTextConversation(peer, { signal })
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

export async function simulateInboundText(
  payload: InboundTextSimulationRequest,
  signal?: AbortSignal
): Promise<TextMessageDetail> {
  return unwrapProtectedResponse(
    await adminApi.simulateInboundText(payload, { signal })
  );
}

export async function saveTextDestination({
  provider,
  localNumber,
  callbackUrl,
  signal,
}: {
  provider: TextProvider;
  localNumber: string;
  callbackUrl: string;
  signal?: AbortSignal;
}): Promise<void> {
  unwrapProtectedResponse(
    await adminApi.putTextDestination(
      provider,
      localNumber,
      { callback_url: callbackUrl },
      { signal }
    )
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
