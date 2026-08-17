import { Link } from '@askrjs/askr/router';
import {
  Breadcrumb,
  BreadcrumbItem,
  BreadcrumbLink,
  BreadcrumbList,
  BreadcrumbPage,
  BreadcrumbSeparator,
} from '@askrjs/themes/components';
import { blobParentPrefix } from '@/features/storage';
import { bucketFolderPath, bucketPath } from '@/shared/routes';

export default function BlobBreadcrumbs({
  blobKey,
  bucketName,
}: {
  blobKey: string;
  bucketName: string;
}) {
  const parentPrefix = blobParentPrefix(blobKey);

  return (
    <Breadcrumb aria-label="Blob path">
      <BreadcrumbList>
        <BreadcrumbItem>
          <BreadcrumbLink asChild>
            <Link href={bucketPath(bucketName)}>{bucketName}</Link>
          </BreadcrumbLink>
        </BreadcrumbItem>
        <BreadcrumbSeparator />
        <BreadcrumbItem>
          {parentPrefix ? (
            <BreadcrumbLink asChild>
              <Link href={bucketFolderPath(bucketName, parentPrefix)}>
                {parentPrefix.replace(/\/$/, '')}
              </Link>
            </BreadcrumbLink>
          ) : (
            <BreadcrumbPage>root</BreadcrumbPage>
          )}
        </BreadcrumbItem>
      </BreadcrumbList>
    </Breadcrumb>
  );
}
