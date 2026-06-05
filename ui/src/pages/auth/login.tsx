import { state } from '@askrjs/askr';
import { navigate } from '@askrjs/askr/router';
import { Input } from '@askrjs/ui';
import { Button, Field } from '@askrjs/themes/controls';
import { Container, Section, Stack } from '@askrjs/themes/layouts';
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from '@askrjs/themes/surfaces';
import { loginAdminSession } from '../../features/auth/admin-session';
import { adminBucketsPath } from '../../shared/routes';

function returnPath(): string {
  if (typeof window === 'undefined') {
    return adminBucketsPath();
  }

  const candidate = new URLSearchParams(window.location.search).get('next');
  return candidate?.startsWith('/') && !candidate.startsWith('//')
    ? candidate
    : adminBucketsPath();
}

export default function LoginPage() {
  const [error, setError] = state('');
  const [pending, setPending] = state(false);

  async function handleSubmit(event: Event) {
    if (pending()) {
      return;
    }

    const target = event.target instanceof Element ? event.target : null;
    const form = target?.closest('form');

    if (!(form instanceof HTMLFormElement)) {
      return;
    }

    const usernameInput = form.querySelector('#username');
    const passwordInput = form.querySelector('#password');
    const credentials = {
      username:
        usernameInput instanceof HTMLInputElement
          ? usernameInput.value.trim()
          : '',
      password:
        passwordInput instanceof HTMLInputElement ? passwordInput.value : '',
    };

    setPending(true);
    setError('');

    try {
      await loginAdminSession(credentials);
      navigate(returnPath());
    } catch (caughtError) {
      setError(
        caughtError instanceof Error
          ? caughtError.message
          : 'The admin server is unavailable right now.'
      );
    } finally {
      setPending(false);
    }
  }

  return (
    <Section size="4">
      <Container size="sm">
        <Card variant="raised">
          <CardHeader>
            <CardTitle>Sign in</CardTitle>
          </CardHeader>
          <CardContent>
            <form
              onSubmit={(event: Event) => {
                event.preventDefault();
                void handleSubmit(event);
              }}
            >
              <Stack gap="4">
                <Field>
                  <label htmlFor="username">Username</label>
                  <Input
                    id="username"
                    name="username"
                    type="text"
                    autoComplete="username"
                    disabled={pending()}
                    placeholder="username"
                  />
                </Field>
                <Field>
                  <label htmlFor="password">Password</label>
                  <Input
                    id="password"
                    name="password"
                    type="password"
                    autoComplete="current-password"
                    disabled={pending()}
                    placeholder="password"
                  />
                </Field>
                {error() ? <p role="alert">{error()}</p> : null}
                <Button type="submit" disabled={pending()}>
                  {pending() ? 'Signing in...' : 'Sign in'}
                </Button>
              </Stack>
            </form>
          </CardContent>
        </Card>
      </Container>
    </Section>
  );
}
