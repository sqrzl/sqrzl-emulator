import { state } from '@askrjs/askr';

export type DeleteTarget<Id> = Id & {
  deleting: boolean;
  error: string;
  pendingCount: boolean;
  count: number | null;
};

export type DeleteTargetController<Id> = {
  target: () => DeleteTarget<Id> | null;
  open: (id: Id) => void;
  confirm: () => void;
  cancel: () => void;
};

function errorMessage(error: unknown, fallback: string): string {
  return error instanceof Error ? error.message : fallback;
}

export function useDeleteTarget<Id extends Record<string, unknown>>(config: {
  keyOf: (id: Id) => string;
  remove: (id: Id) => Promise<void>;
  precount?: (id: Id, signal: AbortSignal) => Promise<number>;
  onDeleted?: () => void | Promise<void>;
  removeError?: string;
  countError?: string;
}): DeleteTargetController<Id> {
  const [target, setTarget] = state<DeleteTarget<Id> | null>(null);

  function open(id: Id) {
    setTarget(() => ({
      ...id,
      deleting: false,
      error: '',
      pendingCount: Boolean(config.precount),
      count: null,
    }));

    if (!config.precount) {
      return;
    }

    const key = config.keyOf(id);
    void (async () => {
      try {
        const count = await config.precount!(id, new AbortController().signal);
        setTarget((value) =>
          value && config.keyOf(value) === key
            ? { ...value, count, pendingCount: false }
            : value
        );
      } catch (caughtError) {
        setTarget((value) =>
          value && config.keyOf(value) === key
            ? {
                ...value,
                pendingCount: false,
                error: errorMessage(
                  caughtError,
                  config.countError ?? 'Count could not be loaded.'
                ),
              }
            : value
        );
      }
    })();
  }

  function confirm() {
    const current = target();
    if (!current || current.deleting || current.pendingCount) {
      return;
    }

    setTarget(() => ({ ...current, deleting: true, error: '' }));

    void (async () => {
      try {
        await config.remove(current);
        setTarget(() => null);
        await config.onDeleted?.();
      } catch (caughtError) {
        setTarget((value) =>
          value
            ? {
                ...value,
                deleting: false,
                error: errorMessage(
                  caughtError,
                  config.removeError ?? 'Item could not be deleted.'
                ),
              }
            : value
        );
      }
    })();
  }

  function cancel() {
    setTarget(() => null);
  }

  return { target, open, confirm, cancel };
}
