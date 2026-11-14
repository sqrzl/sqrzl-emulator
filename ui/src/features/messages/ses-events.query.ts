import { adminApi } from '@/adapters';
import type {
  PutSesEventDestinationRequest,
  SimulateSesEventRequest,
} from '@/adapters/api.g';
import { unwrapProtectedResponse } from '@/features/auth';

export async function getSesDestination(
  configurationSet: string,
  signal: AbortSignal
) {
  const response = await adminApi.getSesEventDestination(configurationSet, {
    signal,
  });
  if (response.status === 404) return null;
  return unwrapProtectedResponse(response);
}
export async function saveSesDestination(
  configurationSet: string,
  body: PutSesEventDestinationRequest
) {
  return unwrapProtectedResponse(
    await adminApi.putSesEventDestination(configurationSet, body)
  );
}
export async function deleteSesDestination(configurationSet: string) {
  unwrapProtectedResponse(
    await adminApi.deleteSesEventDestination(configurationSet)
  );
}
export async function listSesEvents(
  messageId: string,
  signal: AbortSignal,
  next?: string
) {
  return unwrapProtectedResponse(
    await adminApi.listSesEvents(
      { message_id: messageId, next, limit: 50 },
      { signal }
    )
  );
}
export async function simulateSesEvent(body: SimulateSesEventRequest) {
  return unwrapProtectedResponse(await adminApi.simulateSesEvent(body));
}
