import { useCallback, useEffect, useRef, useState } from 'react';

type Inventory<T> =
  | { status: 'loading' }
  | { status: 'ready'; data: T; refreshError?: string }
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
  { keepPrevious = false, refreshKey = 0, queueRefresh = false } = {},
) {
  const [state, setState] = useState<Inventory<T>>({ status: 'loading' });
  const [attempt, setAttempt] = useState(0);
  const [refreshing, setRefreshing] = useState(false);
  const inFlight = useRef(true);
  const queued = useRef(false);
  const startRefresh = useCallback(() => {
    inFlight.current = true;
    setState((previous) => keepPrevious && previous.status === 'ready'
      ? (queueRefresh ? previous : { status: 'ready', data: previous.data })
      : { status: 'loading' });
    setRefreshing(true);
    setAttempt((value) => value + 1);
  }, [keepPrevious, queueRefresh]);

  const refresh = useCallback(() => {
    if (queueRefresh && inFlight.current) {
      queued.current = true;
      return;
    }
    startRefresh();
  }, [queueRefresh, startRefresh]);

  useEffect(() => {
    const controller = new AbortController();
    inFlight.current = true;
    const request = attempt === 0 ? load : reload;
    request(controller.signal).then(
      (data) => {
        if (!controller.signal.aborted) setState({ status: 'ready', data });
      },
      (error: unknown) => {
        if (controller.signal.aborted) return;
        const message = requestError(error);
        setState((previous) => keepPrevious && previous.status === 'ready'
          ? { ...previous, refreshError: message } : { status: 'error', message });
      },
    ).finally(() => {
      if (controller.signal.aborted) return;
      inFlight.current = false;
      if (queued.current) {
        queued.current = false;
        startRefresh();
      } else {
        setRefreshing(false);
      }
    });
    return () => controller.abort();
  }, [attempt, keepPrevious, load, reload, refreshKey, startRefresh]);

  return { state, refresh, refreshing };
}
