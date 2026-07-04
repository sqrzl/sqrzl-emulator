import { Show } from '@askrjs/askr/control';
import {
  Button,
  DataTable,
  EmptyState,
  Inline,
  Spinner,
  Stack,
  Toolbar,
} from '@askrjs/themes/components';
import CursorPagination from './cursor-pagination';
import StorageSearchForm from './storage-search-form';

export default function DataTableSection({
  title,
  searchInputId,
  searchLabel,
  searchValue,
  onSearch,
  loading,
  errored,
  empty,
  emptyTitle,
  emptyDescription,
  errorTitle,
  errorDescription,
  onRetry,
  hasNext,
  hasPrevious,
  onNext,
  onPrevious,
  tableWidth = 'default',
  children,
}: {
  title?: string;
  searchInputId: string;
  searchLabel: string;
  searchValue: string;
  onSearch: (value: string) => void;
  loading: boolean;
  errored: boolean;
  empty: boolean;
  emptyTitle: string;
  emptyDescription: string;
  errorTitle: string;
  errorDescription: string;
  onRetry: () => void;
  hasNext: boolean;
  hasPrevious: boolean;
  onNext: () => void;
  onPrevious: () => void;
  tableWidth?: 'default' | 'wide';
  children?: unknown;
}) {
  const titleId = title ? `${searchInputId}-title` : undefined;

  return (
    <section
      data-sqrzl-slot="storage-data-section"
      aria-labelledby={titleId}
      aria-label={title ?? searchLabel}
    >
      <Stack gap="4">
        <Stack gap="3">
          <Show when={title}>
            <Toolbar title={<span id={titleId}>{title}</span>} />
          </Show>
          <StorageSearchForm
            inputId={searchInputId}
            label={searchLabel}
            defaultValue={searchValue}
            onSearch={onSearch}
          />
        </Stack>

        <Show when={errored}>
          <EmptyState
            title={errorTitle}
            description={errorDescription}
            actions={<Button onPress={onRetry}>Retry</Button>}
          />
        </Show>

        <Show when={!errored && loading}>
          <Inline justify="center" align="center">
            <Spinner />
          </Inline>
        </Show>

        <Show when={!errored && !loading && empty}>
          <EmptyState title={emptyTitle} description={emptyDescription} />
        </Show>

        <Show when={!errored && !loading && !empty}>
          <Stack gap="3">
            <DataTable
              data-sqrzl-slot="storage-table-scroll"
              data-sqrzl-table-width={tableWidth}
            >
              {children}
            </DataTable>
            <CursorPagination
              hasNext={hasNext}
              hasPrevious={hasPrevious}
              onNext={onNext}
              onPrevious={onPrevious}
            />
          </Stack>
        </Show>
      </Stack>
    </section>
  );
}
