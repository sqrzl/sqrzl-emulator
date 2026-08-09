export function formatRelativeTime(value: string): string {
  const date = new Date(value);
  const diffMs = Date.now() - date.getTime();
  const diffMinutes = Math.max(1, Math.round(diffMs / 60000));

  if (diffMinutes < 60) {
    return `${diffMinutes}m ago`;
  }

  const diffHours = Math.round(diffMinutes / 60);
  return `${diffHours}h ago`;
}

export function formatBytes(size: number): string {
  if (size < 1024) {
    return `${size} B`;
  }

  const kib = size / 1024;
  if (kib < 1024) {
    return `${kib.toFixed(kib >= 10 ? 0 : 1)} KiB`;
  }

  const mib = kib / 1024;
  return `${mib.toFixed(mib >= 10 ? 0 : 1)} MiB`;
}

export function formatByteCount(size: number): string {
  return `${formatBytes(size)} (${size.toLocaleString()} bytes)`;
}

export function formatTextProvider(provider: string): string {
  switch (provider) {
    case 'acs':
      return 'Azure Communication Services';
    case 'aws-sms-voice-v2':
      return 'AWS SMS Voice v2';
    case 'sns':
      return 'Amazon SNS';
    case 'twilio':
      return 'Twilio';
    default:
      return provider;
  }
}
