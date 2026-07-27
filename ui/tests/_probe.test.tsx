import { cleanupApp, createSPA } from '@askrjs/askr/boot';
import { createQuery } from '@askrjs/askr/data';
import { createRouteRegistry, route } from '@askrjs/askr/router';
import { describe, expect, it } from 'vite-plus/test';

declare global {
  var __probeValue: string | undefined;
}

function Child() {
  const q = createQuery<string>({
    key: 'probe',
    fetch: async () => globalThis.__probeValue ?? 'none',
  });
  return <p>{q.loading ? 'loading' : q.data}</p>;
}

function App() {
  return <Child />;
}

const probeRegistry = createRouteRegistry(() => {
  route('/', App);
});

async function mount(root: HTMLDivElement): Promise<void> {
  window.history.pushState(null, '', '/');
  await createSPA({ root, registry: probeRegistry });
}

async function flush() {
  await new Promise((r) => setTimeout(r, 0));
  await new Promise((r) => setTimeout(r, 0));
}

describe('probe', () => {
  it('first mount sees A', async () => {
    (globalThis as any).__probeValue = 'A';
    const root = document.createElement('div');
    document.body.appendChild(root);
    await mount(root);
    await flush();
    expect(root.textContent).toBe('A');
    cleanupApp(root);
    root.remove();
  });

  it('second mount sees B (cache evicted)', async () => {
    (globalThis as any).__probeValue = 'B';
    const root = document.createElement('div');
    document.body.appendChild(root);
    await mount(root);
    await flush();
    expect(root.textContent).toBe('B');
    cleanupApp(root);
    root.remove();
  });
});
