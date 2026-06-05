import { Show } from '@askrjs/askr/control';
import { Button } from '@askrjs/themes/controls';
import { EmptyState, Spinner } from '@askrjs/themes/feedback';
import { Inline, Stack } from '@askrjs/themes/layouts';
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
  children?: unknown;
}) {
  const titleId = title ? `${searchInputId}-title` : undefined;

  return (
    <section aria-labelledby={titleId} aria-label={title ?? searchLabel}>
      <Stack gap="4">
        <Stack gap="3">
          <Show when={title}>
            <h2 id={titleId}>{title}</h2>
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
            {children}
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
