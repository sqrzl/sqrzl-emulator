import { adminApi } from '../../adapters';
import type { MailboxInfo } from '../../adapters/api.g';
import { unwrapProtectedResponse } from '../auth/admin-session';

export type MailboxPage = {
  items: MailboxInfo[];
  next: string | null;
};

export async function listMailboxPage({
  next,
  search,
  signal,
}: {
  next?: string;
  search?: string;
  signal: AbortSignal;
}): Promise<MailboxPage> {
  const result = unwrapProtectedResponse(
    await adminApi.listMailboxes(
      { next, limit: 50, search: search?.trim() || undefined },
      { signal }
    )
  );

  return {
    items: result.items,
    next: result.next,
  };
}

export async function deleteMailbox({
  mailbox,
  signal,
}: {
  mailbox: string;
  signal?: AbortSignal;
}): Promise<void> {
  unwrapProtectedResponse(await adminApi.deleteMailbox(mailbox, { signal }));
}
