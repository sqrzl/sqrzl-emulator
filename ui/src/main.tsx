import { createSPA } from '@askrjs/askr/boot';
import { routeRegistry } from './pages/_routes';

import './styles.css';

await createSPA({
  root: document.getElementById('app')!,
  registry: routeRegistry,
});
