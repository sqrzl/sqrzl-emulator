export const textConversationListKey = 'text-conversations';

export function textMessageListKey(peer: string): string {
  return `text-messages:${peer}`;
}

export function textMessageDetailKey(peer: string, messageId: string): string {
  return `text-message:${peer}:${messageId}`;
}
