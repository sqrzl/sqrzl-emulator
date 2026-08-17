import type {
  ObjectFolderInfo as FolderRow,
  ObjectInfo as BlobInfo,
} from '@/adapters/api.g';

export type { FolderRow };

export type BlobBrowserRow =
  | { type: 'folder'; folder: FolderRow }
  | { type: 'blob'; blob: BlobInfo };

export function splitBlobBrowserRows(rows: BlobBrowserRow[]): {
  folders: FolderRow[];
  blobs: BlobInfo[];
} {
  const folders: FolderRow[] = [];
  const blobs: BlobInfo[] = [];

  for (const row of rows) {
    if (row.type === 'folder') {
      folders.push(row.folder);
    } else {
      blobs.push(row.blob);
    }
  }

  return { folders, blobs };
}
