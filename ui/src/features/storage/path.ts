import { bucketFolderPath, bucketPath } from '@/shared/routes';

export type FolderCrumb = {
  label: string;
  prefix: string;
  current: boolean;
};

function decodePathSegment(segment: string): string {
  try {
    return decodeURIComponent(segment);
  } catch {
    return segment;
  }
}

export function normalizeStoragePathPrefix(pathPrefix: string): string {
  const normalized = pathPrefix
    .trim()
    .split('/')
    .filter(Boolean)
    .map(decodePathSegment)
    .join('/');

  return normalized ? `${normalized}/` : '';
}

export function parentStoragePathPrefix(pathPrefix: string): string {
  const trimmed = normalizeStoragePathPrefix(pathPrefix).replace(/\/$/, '');
  const lastSlash = trimmed.lastIndexOf('/');
  if (lastSlash < 0) {
    return '';
  }

  return `${trimmed.slice(0, lastSlash + 1)}`;
}

export function storagePathSegments(pathPrefix: string): string[] {
  return normalizeStoragePathPrefix(pathPrefix)
    .replace(/\/$/, '')
    .split('/')
    .filter(Boolean);
}

export function storagePathCrumbs(pathPrefix: string): FolderCrumb[] {
  const segments = storagePathSegments(pathPrefix);
  let prefix = '';

  return segments.map((segment, index) => {
    prefix = `${prefix}${segment}/`;
    return {
      label: segment,
      prefix,
      current: index === segments.length - 1,
    };
  });
}

export function storagePathLabel(fallback: string, pathPrefix: string): string {
  const segments = storagePathSegments(pathPrefix);
  return segments.length > 0 ? segments[segments.length - 1] : fallback;
}

export function blobFileName(blobKey: string): string {
  return blobKey.split('/').filter(Boolean).pop() ?? blobKey;
}

export function blobParentPrefix(blobKey: string): string {
  const slashIndex = blobKey.lastIndexOf('/');
  return slashIndex < 0 ? '' : blobKey.slice(0, slashIndex + 1);
}

export function blobParentPath(bucketName: string, blobKey: string): string {
  const parentPrefix = blobParentPrefix(blobKey);
  return parentPrefix
    ? bucketFolderPath(bucketName, parentPrefix)
    : bucketPath(bucketName);
}

export function resolveUploadObjectKey({
  fileName,
  pathPrefix,
  typedKey,
}: {
  fileName: string;
  pathPrefix: string;
  typedKey: string;
}): string {
  const normalizedPrefix = normalizeStoragePathPrefix(pathPrefix);
  const normalizedKey = (typedKey || fileName).trim().replace(/^\/+/, '');

  if (!normalizedPrefix || normalizedKey.startsWith(normalizedPrefix)) {
    return normalizedKey;
  }

  return `${normalizedPrefix}${normalizedKey}`;
}
