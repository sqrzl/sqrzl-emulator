import { group, registerRoutes } from '@askrjs/askr/router';
import RootLayout from './_layout';
import { registerAppRoutes } from './app/_routes';
import AppLayout from './app/_layout';
import { resolveAdminSession } from '../features/auth/admin-session';
import { registerAuthRoutes } from './auth/_routes';
import { adminBucketsPath, loginPath } from '../shared/routes';

registerRoutes(
  () => {
    group({ layout: RootLayout }, () => {
      registerAuthRoutes();

      group({ layout: AppLayout, auth: true }, () => {
        registerAppRoutes();
      });
    });
  },
  {
    auth: {
      resolve: resolveAdminSession,
      loginPath: (context) =>
        `${loginPath()}?next=${encodeURIComponent(context.href)}`,
      guestRedirectTo: adminBucketsPath(),
    },
  }
);
