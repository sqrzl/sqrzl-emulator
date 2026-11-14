import { group, route } from '@askrjs/askr/router';
import { requireAnonymous } from '@askrjs/auth';
import LoginPage from './login';
import LogoutPage from './logout';

export function registerGuestRoutes(): void {
  route('/', LoginPage);
  route('/login', LoginPage);
  route('/auth', LoginPage);
  route('/auth/login', LoginPage);
}

export function registerLogoutRoute(): void {
  route('/logout', LogoutPage);
  route('/auth/logout', LogoutPage);
}

export function registerAuthRoutes(): void {
  group({ auth: requireAnonymous() }, registerGuestRoutes);
  registerLogoutRoute();
}
