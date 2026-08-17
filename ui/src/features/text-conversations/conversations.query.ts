import { adminApi } from '@/adapters';
import type { TextConversation } from '@/adapters/api.g';
import { unwrapProtectedResponse } from '@/features/auth';

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
