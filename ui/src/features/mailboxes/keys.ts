export const mailboxListKey = 'mailboxes';

export function mailboxMessagesListKey(mailbox: string): string {
  return `mailbox-messages:${mailbox}`;
}
