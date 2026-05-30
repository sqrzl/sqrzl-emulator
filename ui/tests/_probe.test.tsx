import { cleanupApp, createIsland } from '@askrjs/askr/boot';
import { createQuery } from '@askrjs/askr/data';
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

async function flush() {
  await new Promise((r) => setTimeout(r, 0));
  await new Promise((r) => setTimeout(r, 0));
}

describe('probe', () => {
  it('first mount sees A', async () => {
    (globalThis as any).__probeValue = 'A';
    const root = document.createElement('div');
    document.body.appendChild(root);
    createIsland({ root, component: App });
    await flush();
    expect(root.textContent).toBe('A');
    cleanupApp(root);
    root.remove();
  });

  it('second mount sees B (cache evicted)', async () => {
    (globalThis as any).__probeValue = 'B';
    const root = document.createElement('div');
    document.body.appendChild(root);
    createIsland({ root, component: App });
    await flush();
    expect(root.textContent).toBe('B');
    cleanupApp(root);
    root.remove();
  });
});
