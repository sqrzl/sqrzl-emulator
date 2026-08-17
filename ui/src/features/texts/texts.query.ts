import { adminApi } from '@/adapters';
import type {
  InboundTextSimulationRequest,
  TextMessageDetail,
  TextProvider,
} from '@/adapters/api.g';
import { unwrapProtectedResponse } from '@/features/auth';

export async function simulateInboundText(
  payload: InboundTextSimulationRequest,
  signal?: AbortSignal
): Promise<TextMessageDetail> {
  return unwrapProtectedResponse(
    await adminApi.simulateInboundText(payload, { signal })
  );
}

export async function saveTextDestination({
  provider,
  localNumber,
  callbackUrl,
  signal,
}: {
  provider: TextProvider;
  localNumber: string;
  callbackUrl: string;
  signal?: AbortSignal;
}): Promise<void> {
  unwrapProtectedResponse(
    await adminApi.putTextDestination(
      provider,
      localNumber,
      { callback_url: callbackUrl },
      { signal }
    )
  );
}
