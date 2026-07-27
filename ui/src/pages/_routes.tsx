import { createRouteRegistry, group } from '@askrjs/askr/router';
import RootLayout from './_layout';
import { registerAppRoutes } from './app/_routes';
import AppLayout from './app/_layout';
import { resolveAdminSession } from '../features/auth/admin-session';
import { registerAuthRoutes } from './auth/_routes';
import { requireUser } from '@askrjs/auth';
import { adminBucketsPath, loginPath } from '../shared/routes';

export const routeRegistry = createRouteRegistry(
  () => {
    group({ layout: RootLayout }, () => {
      registerAuthRoutes();

      group({ layout: AppLayout, auth: requireUser() }, () => {
        registerAppRoutes();
      });
    });
  },
  {
    auth: {
      resolve: resolveAdminSession,
      loginPath: (context) =>
        `${loginPath()}?next=${encodeURIComponent(context.href)}`,
      authenticatedRedirectTo: adminBucketsPath(),
    },
  }
);
