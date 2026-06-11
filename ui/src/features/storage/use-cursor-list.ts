import { state } from '@askrjs/askr';
import { createQuery } from '@askrjs/askr/data';
import { currentRoute, navigate } from '@askrjs/askr/router';
import { resource } from '@askrjs/askr/resources';

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

type CursorListFetch<T> = (opts: {
  signal: AbortSignal;
}) => Promise<CursorPage<T>>;

const fetchByQueryKey = new Map<string, CursorListFetch<unknown>>();
const fetchByQueryKeyLimit = 256;

function cacheFetchForQueryKey<T>(key: string, fetch: CursorListFetch<T>): CursorListFetch<T> {
  const cached = fetchByQueryKey.get(key) as CursorListFetch<T> | undefined;
  if (cached) {
    return cached;
  }

  if (fetchByQueryKey.size >= fetchByQueryKeyLimit) {
    const oldestKey = fetchByQueryKey.keys().next().value as string | undefined;
    if (oldestKey !== undefined) {
      fetchByQueryKey.delete(oldestKey);
    }
  }

  fetchByQueryKey.set(key, fetch as CursorListFetch<unknown>);
  return fetch;
}
function currentSearchFromRoute(queryParam: string): string {
  return currentRoute().query.get(queryParam)?.trim() ?? '';
}

function serializeRouteQuery(
  query: ReturnType<typeof currentRoute>['query']
): string {
  const nextSearch = new URLSearchParams();
  const entries = query.toJSON();

  for (const [key, value] of Object.entries(entries)) {
    if (Array.isArray(value)) {
      for (const item of value) {
        nextSearch.append(key, item);
      }
      continue;
    }

    nextSearch.append(key, value);
  }

  return nextSearch.toString();
}

function writeSearchParam(
  route: ReturnType<typeof currentRoute>,
  queryParam: string,
  nextValue: string
): void {
  const trimmed = nextValue.trim();
  const nextSearch = new URLSearchParams(serializeRouteQuery(route.query));

  if (trimmed) {
    nextSearch.set(queryParam, trimmed);
  } else {
    nextSearch.delete(queryParam);
  }

  const nextHref = `${route.path}${
    nextSearch.toString() ? `?${nextSearch}` : ''
  }${route.hash ? `#${route.hash}` : ''}`;
  const currentQuery = serializeRouteQuery(route.query);
  const currentHref = `${route.path}${
    currentQuery ? `?${currentQuery}` : ''
  }${route.hash ? `#${route.hash}` : ''}`;

  if (nextHref !== currentHref) {
    navigate(nextHref, { history: 'replace' });
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
  const route = currentRoute();
  const [search, setSearchValue] = state(currentSearchFromRoute(queryParam));
  const [cursor, setCursor] = state<string | undefined>(undefined);
  const [history, setHistory] = state<Array<string | undefined>>([]);
  const routeSearch = route.query.get(queryParam)?.trim() ?? '';
  const activeSearch = search();
  const routeSearchSynced = routeSearch === activeSearch;
  const currentCursor = routeSearchSynced ? cursor() : undefined;
  const currentSearch = routeSearch;
  const queryKey = `${keyPrefix}:search=${currentSearch}:cursor=${currentCursor ?? ''}`;

  const query = createQuery<CursorPage<T>>({
    key: queryKey,
    fetch: cacheFetchForQueryKey(queryKey, ({ signal }) =>
      fetchPage({
        next: currentCursor,
        search: routeSearch || undefined,
        signal,
      })
    ),
  });

  function setSearch(value: string) {
    const nextValue = value.trim();
    if (nextValue === routeSearch && nextValue === activeSearch) {
      return;
    }

    writeSearchParam(route, queryParam, nextValue);
    setSearchValue(nextValue);
    setCursor(undefined);
    setHistory([]);
  }

  resource(() => {
    if (routeSearch === search()) {
      return null;
    }

    setSearchValue(routeSearch);
    setCursor(undefined);
    setHistory([]);
    return null;
  }, [routeSearch]);

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
