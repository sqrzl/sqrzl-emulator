const relativeTimeFormatter = new Intl.RelativeTimeFormat(undefined, {
  numeric: 'auto',
});

export function formatRelativeTime(value: string): string {
  const date = new Date(value);
  const diffSeconds = (date.getTime() - Date.now()) / 1000;
  const absoluteSeconds = Math.abs(diffSeconds);

  if (!Number.isFinite(diffSeconds)) {
    return 'Unknown';
  }

  if (absoluteSeconds < 45) {
    return 'just now';
  }

  let unit: Intl.RelativeTimeFormatUnit;
  let unitSeconds: number;

  if (absoluteSeconds < 45 * 60) {
    unit = 'minute';
    unitSeconds = 60;
  } else if (absoluteSeconds < 22 * 60 * 60) {
    unit = 'hour';
    unitSeconds = 60 * 60;
  } else if (absoluteSeconds < 7 * 24 * 60 * 60) {
    unit = 'day';
    unitSeconds = 24 * 60 * 60;
  } else if (absoluteSeconds < 30 * 24 * 60 * 60) {
    unit = 'week';
    unitSeconds = 7 * 24 * 60 * 60;
  } else if (absoluteSeconds < 365 * 24 * 60 * 60) {
    unit = 'month';
    unitSeconds = 30 * 24 * 60 * 60;
  } else {
    unit = 'year';
    unitSeconds = 365 * 24 * 60 * 60;
  }

  return relativeTimeFormatter.format(
    Math.round(diffSeconds / unitSeconds),
    unit
  );
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
