import { createRouteRegistry, fallback, group } from '@askrjs/askr/router';
import NotFoundPage from './not-found';
import RootLayout from './_layout';
import { registerAppRoutes } from './app/_routes';
import AppLayout from './app/_layout';
import { resolveAdminSession } from '@/features/auth';
import { registerAuthRoutes } from './auth/_routes';
import { requireUser } from '@askrjs/auth';
import { adminBucketsPath, loginPath } from '@/shared/routes';

export const routeRegistry = createRouteRegistry(
  () => {
    group({ layout: RootLayout }, () => {
      registerAuthRoutes();

      group({ layout: AppLayout, auth: requireUser() }, () => {
        registerAppRoutes();
      });
      fallback(NotFoundPage);
    });
  },
  {
    trailingSlash: 'never',
    auth: {
      resolve: resolveAdminSession,
      loginPath: (context) =>
        `${loginPath()}?next=${encodeURIComponent(context.href)}`,
      authenticatedRedirectTo: adminBucketsPath(),
    },
  }
);
