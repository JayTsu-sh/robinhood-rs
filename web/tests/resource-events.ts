import type { Page } from '@playwright/test';

export async function controlResourceEvents(page: Page) {
  await page.addInitScript(() => {
    let reconnecting = false;
    class ResourceEventSource extends EventTarget {
      static readonly CLOSED = 2;
      readyState = 1;
      private forward = (event: Event) => {
        const { type, data } = (event as CustomEvent<{ type: string; data: string }>).detail;
        if (type === 'disconnect') {
          reconnecting = true;
          this.close();
          this.dispatchEvent(new Event('error'));
        } else {
          this.dispatchEvent(new MessageEvent(type, { data }));
        }
      };

      constructor(url: string) {
        super();
        if (url !== '/api/filesystems/events') throw new Error(`Unexpected stream: ${url}`);
        window.addEventListener('test-resource-event', this.forward);
        if (reconnecting) queueMicrotask(() => this.dispatchEvent(new Event('open')));
      }

      close() {
        this.readyState = ResourceEventSource.CLOSED;
        window.removeEventListener('test-resource-event', this.forward);
      }
    }

    Object.defineProperty(window, 'EventSource', { value: ResourceEventSource });
  });

  return (type: 'snapshot' | 'management' | 'disconnect' | 'resync', data = '') => page.evaluate((detail) => {
    window.dispatchEvent(new CustomEvent('test-resource-event', { detail }));
  }, { type, data });
}
