import { useEffect } from 'react';

type RefreshTarget = 'management' | 'both';

export function useResourceEvents(refreshSnapshot: () => void, refreshManagement: () => void, filesystem?: string) {
  useEffect(() => {
    let source: EventSource;
    let batchTimer: number | undefined;
    let reconnectTimer: number | undefined;
    let snapshotPending = false;

    function schedule(target: RefreshTarget) {
      snapshotPending ||= target !== 'management';
      if (batchTimer !== undefined) return;
      batchTimer = window.setTimeout(() => {
        batchTimer = undefined;
        if (snapshotPending) refreshSnapshot();
        refreshManagement();
        snapshotPending = false;
      }, 150);
    }

    function connect() {
      const connection = new EventSource('/api/filesystems/events');
      source = connection;
      connection.addEventListener('open', () => schedule('both'));
      connection.addEventListener('resync', () => schedule('both'));
      connection.addEventListener('snapshot', ({ data }: MessageEvent<string>) => {
        if (filesystem === undefined || filesystem === data) schedule('both');
      });
      connection.addEventListener('management', ({ data }: MessageEvent<string>) => {
        if (data === '*') schedule('management');
        else if (filesystem === undefined || filesystem === data) schedule('both');
      });
      connection.addEventListener('error', () => {
        // HTTP errors can close EventSource permanently. Network failures retry automatically.
        if (connection.readyState === EventSource.CLOSED) reconnectTimer = window.setTimeout(connect, 5_000);
      });
    }

    connect();
    return () => {
      source.close();
      window.clearTimeout(batchTimer);
      window.clearTimeout(reconnectTimer);
    };
  }, [filesystem, refreshManagement, refreshSnapshot]);
}
