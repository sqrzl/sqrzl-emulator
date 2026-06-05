import { state } from '@askrjs/askr';
import { createQuery } from '@askrjs/askr/data';

export type CursorPage<T> = {
  items: T[];
  next: string | null;
};

export type CursorListController<T> = {
  items: () => T[];
  pending: () => boolean;
  error: () => Error | null;
  refresh: () => void;
  search: () => string;
  setSearch: (value: string) => void;
  hasNext: () => boolean;
  hasPrevious: () => boolean;
  next: () => void;
  previous: () => void;
};

function readQueryParam(name: string): string {
  if (typeof window === 'undefined') {
    return '';
  }

  return window.location.search
    ? new URLSearchParams(window.location.search).get(name)?.trim() ?? ''
    : '';
}

function writeQueryParam(name: string, value: string): void {
  if (typeof window === 'undefined') {
    return;
  }

  const url = new URL(window.location.href);
  const nextValue = value.trim();

  if (nextValue) {
    url.searchParams.set(name, nextValue);
  } else {
    url.searchParams.delete(name);
  }

  const nextHref = `${url.pathname}${url.search}${url.hash}`;
  const currentHref = `${window.location.pathname}${window.location.search}${window.location.hash}`;
  if (nextHref !== currentHref) {
    window.history.pushState({}, '', nextHref);
  }
}

export function useCursorList<T>(
  keyPrefix: string,
  queryParam: string,
  fetchPage: (opts: {
    next?: string;
    search?: string;
    signal: AbortSignal;
  }) => Promise<CursorPage<T>>
): CursorListController<T> {
  const [search, setSearchValue] = state(readQueryParam(queryParam));
  const [cursor, setCursor] = state<string | undefined>(undefined);
  const [history, setHistory] = state<Array<string | undefined>>([]);

  const query = createQuery<CursorPage<T>>({
    key: `${keyPrefix}:search=${search()}:cursor=${cursor() ?? ''}`,
    fetch: ({ signal }) =>
      fetchPage({
        next: cursor(),
        search: search() || undefined,
        signal,
      }),
  });

  function setSearch(value: string) {
    const nextValue = value.trim();
    if (nextValue === search()) {
      return;
    }

    writeQueryParam(queryParam, nextValue);
    setSearchValue(nextValue);
    setCursor(undefined);
    setHistory([]);
  }

  function next() {
    const token = query.data?.next;
    if (!token) {
      return;
    }
    setHistory((stack) => [...stack, cursor()]);
    setCursor(token);
  }

  function previous() {
    const stack = history();
    if (stack.length === 0) {
      return;
    }
    const previousCursor = stack[stack.length - 1];
    setHistory(stack.slice(0, -1));
    setCursor(previousCursor);
  }

  return {
    items: () => query.data?.items ?? [],
    pending: () => query.loading,
    error: () => (query.error as Error | null) ?? null,
    refresh: () => void query.refresh(),
    search,
    setSearch,
    hasNext: () => Boolean(query.data?.next),
    hasPrevious: () => history().length > 0,
    next,
    previous,
  };
}
