/**
 * 中枢通知桥：监听后端 central-notification 事件并转为应用内 toast
 * （send_notification 工具的落地端；挂载于 ToastProvider 内）。
 */
import { useEffect } from 'react';
import { listen } from '@tauri-apps/api/event';
import { useToast } from '../Toast';

interface CentralNotification {
  title: string;
  message?: string;
  level?: string;
}

export function CentralNotificationBridge() {
  const { showInfo, showWarning, showError } = useToast();
  useEffect(() => {
    let disposed = false;
    let unsubscribe: (() => void) | undefined;
    void listen<CentralNotification>('central-notification', (event) => {
      const { title, message, level } = event.payload ?? {};
      if (!title) return;
      if (level === 'error') {
        showError(title, message);
      } else if (level === 'warning') {
        showWarning(title, message);
      } else {
        showInfo(title, message);
      }
    }).then((un) => {
      if (disposed) {
        un();
      } else {
        unsubscribe = un;
      }
    });
    return () => {
      disposed = true;
      unsubscribe?.();
    };
  }, [showInfo, showWarning, showError]);
  return null;
}
