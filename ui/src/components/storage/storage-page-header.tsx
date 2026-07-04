import { PageHeader } from '@askrjs/themes/components';

export default function StoragePageHeader({
  actions,
  description,
  title,
}: {
  actions?: unknown;
  description?: string;
  title: string;
}) {
  return (
    <PageHeader
      data-sqrzl-slot="storage-page-header"
      title={title}
      description={description}
      actions={actions}
    />
  );
}
