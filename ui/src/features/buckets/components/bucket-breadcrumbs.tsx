import { For } from '@askrjs/askr/control';
import { Link } from '@askrjs/askr/router';
import {
  Breadcrumb,
  BreadcrumbItem,
  BreadcrumbLink,
  BreadcrumbList,
  BreadcrumbPage,
  BreadcrumbSeparator,
} from '@askrjs/themes/components';
import { storagePathCrumbs } from '@/features/storage';
import { adminBucketsPath, bucketFolderPath } from '@/shared/routes';

export default function BucketBreadcrumbs({
  bucketName,
  pathPrefix,
}: {
  bucketName: string;
  pathPrefix: string;
}) {
  const crumbs = storagePathCrumbs(pathPrefix);

  return (
    <Breadcrumb aria-label="Bucket path">
      <BreadcrumbList>
        <BreadcrumbItem>
          <BreadcrumbLink asChild>
            <Link href={adminBucketsPath()}>Buckets</Link>
          </BreadcrumbLink>
        </BreadcrumbItem>
        <BreadcrumbSeparator />
        <BreadcrumbItem>
          {crumbs.length > 0 ? (
            <BreadcrumbLink asChild>
              <Link href={bucketFolderPath(bucketName, '')}>{bucketName}</Link>
            </BreadcrumbLink>
          ) : (
            <BreadcrumbPage>{bucketName}</BreadcrumbPage>
          )}
        </BreadcrumbItem>
        <For each={crumbs} by={(crumb) => crumb.prefix}>
          {(crumb) => (
            <BreadcrumbItem>
              <BreadcrumbSeparator />
              {crumb.current ? (
                <BreadcrumbPage>{crumb.label}</BreadcrumbPage>
              ) : (
                <BreadcrumbLink asChild>
                  <Link href={bucketFolderPath(bucketName, crumb.prefix)}>
                    {crumb.label}
                  </Link>
                </BreadcrumbLink>
              )}
            </BreadcrumbItem>
          )}
        </For>
      </BreadcrumbList>
    </Breadcrumb>
  );
}
