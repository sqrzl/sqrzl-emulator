import { adminApi } from '@/adapters';
import type {
  Acl,
  ListVersionsResponse,
  ObjectFolderInfo,
  ObjectInfo,
  ObjectMetadata,
  TagsResponse,
} from '@/adapters/api.g';
import { blobIdFromBlobKey } from '@/shared/routes';
import { unwrapProtectedResponse } from '@/features/auth';

export type ObjectPage = {
  folders: ObjectFolderInfo[];
  items: ObjectInfo[];
  next: string | null;
};

export type DownloadedObject = {
  blob: Blob;
  contentType: string;
  fileName: string;
};

type ObjectPageVisit = (page: ObjectPage) => boolean | void;

async function walkObjectPages({
  bucketName,
  search,
  pathPrefix,
  signal,
  visit,
}: {
  bucketName: string;
  search?: string;
  pathPrefix?: string;
  signal: AbortSignal;
  visit: ObjectPageVisit;
}): Promise<void> {
  const prefixes = [pathPrefix];
  const includeFolders = !search?.trim();

  for (let index = 0; index < prefixes.length; index += 1) {
    let next: string | undefined;
    const currentPrefix = prefixes[index];

    do {
      const page = await loadObjectPage({
        bucketName,
        next,
        search,
        pathPrefix: currentPrefix,
        signal,
      });

      if (visit(page) === false) {
        return;
      }

      if (includeFolders) {
        prefixes.push(...page.folders.map((folder) => folder.prefix));
      }
      next = page.next ?? undefined;
    } while (next);
  }
}

export async function loadObjectPage({
  bucketName,
  next,
  search,
  pathPrefix,
  signal,
}: {
  bucketName: string;
  next?: string;
  search?: string;
  pathPrefix?: string;
  signal: AbortSignal;
}): Promise<ObjectPage> {
  const page = unwrapProtectedResponse(
    await adminApi.listObjects(
      bucketName,
      {
        next,
        limit: 50,
        search: search?.trim() || undefined,
        prefix: pathPrefix,
      },
      { signal }
    )
  );

  return {
    folders: page.folders ?? [],
    items: page.items,
    next: page.next,
  };
}

export async function loadAllObjectPages({
  bucketName,
  search,
  pathPrefix,
  signal,
}: {
  bucketName: string;
  search?: string;
  pathPrefix?: string;
  signal: AbortSignal;
}): Promise<ObjectInfo[]> {
  const items: ObjectInfo[] = [];

  await walkObjectPages({
    bucketName,
    search,
    pathPrefix,
    signal,
    visit: (page) => {
      items.push(...page.items);
    },
  });

  return items;
}

export async function findObjectByBlobId({
  bucketName,
  blobId,
  pathPrefix,
  signal,
}: {
  bucketName: string;
  blobId: string;
  pathPrefix?: string;
  signal: AbortSignal;
}): Promise<ObjectInfo | undefined> {
  let resolved: ObjectInfo | undefined;

  await walkObjectPages({
    bucketName,
    pathPrefix,
    signal,
    visit: (page) => {
      resolved = page.items.find(
        (object) => blobIdFromBlobKey(object.key) === blobId
      );
      if (resolved) {
        return false;
      }
    },
  });

  return resolved;
}

export async function countBucketObjects({
  bucketName,
  signal,
}: {
  bucketName: string;
  signal: AbortSignal;
}): Promise<number> {
  return (
    await loadAllObjectPages({
      bucketName,
      signal,
    })
  ).length;
}

export async function loadObjectMetadata({
  bucketName,
  objectKey,
  signal,
}: {
  bucketName: string;
  objectKey: string;
  signal: AbortSignal;
}): Promise<ObjectMetadata> {
  return unwrapProtectedResponse(
    await adminApi.getObjectMetadata(bucketName, objectKey, { signal })
  );
}

export async function loadObjectTags({
  bucketName,
  objectKey,
  signal,
}: {
  bucketName: string;
  objectKey: string;
  signal: AbortSignal;
}): Promise<TagsResponse> {
  return unwrapProtectedResponse(
    await adminApi.getObjectTags(bucketName, objectKey, { signal })
  );
}

export async function putObjectTags({
  bucketName,
  objectKey,
  tags,
  signal,
}: {
  bucketName: string;
  objectKey: string;
  tags: Record<string, string>;
  signal?: AbortSignal;
}): Promise<TagsResponse> {
  return unwrapProtectedResponse(
    await adminApi.putObjectTags(bucketName, objectKey, { tags }, { signal })
  );
}

export async function loadObjectAcl({
  bucketName,
  objectKey,
  signal,
}: {
  bucketName: string;
  objectKey: string;
  signal: AbortSignal;
}): Promise<Acl> {
  return unwrapProtectedResponse(
    await adminApi.getObjectAcl(bucketName, objectKey, { signal })
  );
}

export async function saveObjectAcl({
  bucketName,
  objectKey,
  acl,
  signal,
}: {
  bucketName: string;
  objectKey: string;
  acl: Acl;
  signal?: AbortSignal;
}): Promise<Acl> {
  return unwrapProtectedResponse(
    await adminApi.setObjectAcl(bucketName, objectKey, acl, { signal })
  );
}

export async function loadObjectVersions({
  bucketName,
  objectKey,
  next,
  signal,
}: {
  bucketName: string;
  objectKey: string;
  next?: string;
  signal: AbortSignal;
}): Promise<ListVersionsResponse> {
  return unwrapProtectedResponse(
    await adminApi.listObjectVersions(
      bucketName,
      objectKey,
      { next, limit: 25 },
      { signal }
    )
  );
}

export async function putObjectContent({
  bucketName,
  objectKey,
  content,
  contentType,
  metadata,
  signal,
}: {
  bucketName: string;
  objectKey: string;
  content: BodyInit;
  contentType?: string;
  metadata?: Record<string, string>;
  signal?: AbortSignal;
}): Promise<ObjectMetadata> {
  const headers = new Headers();
  headers.set('content-type', contentType ?? 'application/octet-stream');
  Object.entries(metadata ?? {}).forEach(([name, value]) => {
    headers.set(`x-amz-meta-${name}`, value);
  });

  return unwrapProtectedResponse(
    await adminApi.putObjectContent(bucketName, objectKey, content, headers, {
      signal,
    })
  );
}

export async function downloadObjectContent({
  bucketName,
  objectKey,
  signal,
}: {
  bucketName: string;
  objectKey: string;
  signal?: AbortSignal;
}): Promise<DownloadedObject> {
  const response = await adminApi.downloadObjectContent(bucketName, objectKey, {
    signal,
  });
  const content = unwrapProtectedResponse(response) as unknown;
  const contentType =
    response.headers.get('content-type') ?? 'application/octet-stream';
  const blob =
    content instanceof Blob
      ? content
      : new Blob([String(content ?? '')], { type: contentType });

  return {
    blob,
    contentType,
    fileName: objectKey.split('/').filter(Boolean).pop() ?? objectKey,
  };
}

export async function deleteObject({
  bucketName,
  objectKey,
  signal,
}: {
  bucketName: string;
  objectKey: string;
  signal?: AbortSignal;
}): Promise<void> {
  unwrapProtectedResponse(
    await adminApi.deleteObject(bucketName, objectKey, { signal })
  );
}

export async function deleteAllBucketObjects({
  bucketName,
  signal,
}: {
  bucketName: string;
  signal?: AbortSignal;
}): Promise<number> {
  const objects = await loadAllObjectPages({
    bucketName,
    signal: signal ?? new AbortController().signal,
  });

  for (const object of objects) {
    await deleteObject({ bucketName, objectKey: object.key, signal });
  }

  return objects.length;
}

export async function deleteObjectVersion({
  bucketName,
  objectKey,
  versionId,
  signal,
}: {
  bucketName: string;
  objectKey: string;
  versionId: string;
  signal?: AbortSignal;
}): Promise<void> {
  unwrapProtectedResponse(
    await adminApi.deleteObjectVersion(bucketName, objectKey, versionId, {
      signal,
    })
  );
}
