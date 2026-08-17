import type { FetchResponse } from '@fgrzl/fetch';
import { navigate } from '@askrjs/askr/router';
import type { AuthSession } from '@askrjs/auth';
import { adminApi } from '@/adapters';
import type { AdminLoginRequest, AdminSessionResponse } from '@/adapters/api.g';
import { isUnauthorized, unwrapResponse } from '@/adapters/response';

export type AdminSession = AdminSessionResponse;

export type AdminUser = {
  id: string;
  name: string;
  mode: AdminSession['mode'];
};

type AdminAuthContext = {
  authenticated: boolean;
  principal: AdminUser | null;
  session: (AdminSession & AuthSession) | null;
  tenant: string | null;
  user: AdminUser | null;
};

function normalizedSession(session: AdminSession): AdminSession & AuthSession {
  const username = session.username ?? 'local-development';
  return {
    id: username,
    subject: username,
    ...session,
  };
}

export function isDevAuthBypassed(): boolean {
  return (
    import.meta.env.MODE === 'development' &&
    import.meta.env.VITE_REQUIRE_ADMIN_AUTH !== 'true'
  );
}

function localDevelopmentSession(): AdminSession {
  return {
    mode: 'open',
    username: 'local-development',
  };
}

export async function loginAdminSession(
  credentials: AdminLoginRequest,
  signal?: AbortSignal
): Promise<void> {
  if (isDevAuthBypassed()) {
    return;
  }

  unwrapResponse(await adminApi.loginAdminSession(credentials, { signal }));
}

export async function logoutAdminSession({
  signal,
}: {
  signal?: AbortSignal;
} = {}): Promise<void> {
  if (isDevAuthBypassed()) {
    return;
  }

  unwrapResponse(await adminApi.logoutAdminSession({ signal }));
}

export async function loadAdminSession({
  signal,
}: {
  signal?: AbortSignal;
} = {}): Promise<AdminSession> {
  if (isDevAuthBypassed()) {
    return localDevelopmentSession();
  }

  return unwrapResponse(await adminApi.getAdminSession({ signal }));
}

function currentLocationFromWindow(): string {
  if (typeof window === 'undefined') {
    return '/';
  }

  return `${window.location.pathname}${window.location.search}${window.location.hash}`;
}

export function unwrapProtectedResponse<T>(response: FetchResponse<T>): T {
  try {
    return unwrapResponse(response);
  } catch (error) {
    if (
      !isDevAuthBypassed() &&
      isUnauthorized(error) &&
      typeof window !== 'undefined' &&
      /^\/admin(?:\/|$)/.test(window.location.pathname)
    ) {
      const next = currentLocationFromWindow();
      navigate(`/auth?next=${encodeURIComponent(next)}`, {
        history: 'replace',
      });
    }

    throw error;
  }
}

export async function resolveAdminSession({
  signal,
}: {
  signal: AbortSignal;
}): Promise<AdminAuthContext> {
  if (isDevAuthBypassed()) {
    const session = localDevelopmentSession();
    return {
      authenticated: true,
      principal: {
        id: session.username ?? 'local-development',
        name: 'Local development',
        mode: session.mode,
      },
      session: normalizedSession(session),
      tenant: null,
      user: {
        id: session.username ?? 'local-development',
        name: 'Local development',
        mode: session.mode,
      },
    };
  }

  try {
    const session = await loadAdminSession({ signal });
    return {
      authenticated: true,
      principal: {
        id: session.username ?? 'administrator',
        name: session.username ?? 'Local administrator',
        mode: session.mode,
      },
      session: normalizedSession(session),
      tenant: null,
      user: {
        id: session.username ?? 'administrator',
        name: session.username ?? 'Local administrator',
        mode: session.mode,
      },
    };
  } catch (error) {
    if (isUnauthorized(error)) {
      return {
        authenticated: false,
        principal: null,
        session: null,
        tenant: null,
        user: null,
      };
    }

    throw error;
  }
}
