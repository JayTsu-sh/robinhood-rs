import { useCallback, useEffect, useState } from 'react';

type Inventory<T> =
  | { status: 'loading' }
  | { status: 'ready'; data: T }
  | { status: 'error'; message: string };

export function requestError(error: unknown): string {
  const response = (error as { response?: { data?: { error?: unknown }; status?: number } } | null)?.response;
  if (typeof response?.data?.error === 'string') return response.data.error;
  if (response?.status) return `HTTP ${response.status}`;
  return error instanceof Error ? error.message : '无法连接 daemon';
}

export function useInventory<T>(
  load: (signal: AbortSignal) => Promise<T>,
  reload: (signal: AbortSignal) => Promise<T> = load,
) {
  const [state, setState] = useState<Inventory<T>>({ status: 'loading' });
  const [attempt, setAttempt] = useState(0);
  const refresh = useCallback(() => {
    setState({ status: 'loading' });
    setAttempt((value) => value + 1);
  }, []);

  useEffect(() => {
    const controller = new AbortController();
    const request = attempt === 0 ? load : reload;
    request(controller.signal).then(
      (data) => {
        if (!controller.signal.aborted) setState({ status: 'ready', data });
      },
      (error: unknown) => {
        if (!controller.signal.aborted) setState({ status: 'error', message: requestError(error) });
      },
    );
    return () => controller.abort();
  }, [attempt, load, reload]);

  return { state, refresh };
}
