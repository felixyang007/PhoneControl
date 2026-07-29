import { useEffect } from 'react';
import { invoke } from '@tauri-apps/api/core';
import { useStore } from '../store';

interface Lease {
  serial: string;
  task_id: string;
}

// Polls the control API's lease registry so the device grid can badge devices
// a CI smoke task is currently driving. Cheap (a few serials); 2s cadence.
export function useLeases(intervalMs = 2000) {
  const setLeasedSerials = useStore((s) => s.setLeasedSerials);

  useEffect(() => {
    let alive = true;
    const poll = async () => {
      try {
        const leases = await invoke<Lease[]>('get_leases');
        if (!alive) return;
        const map: Record<string, string> = {};
        for (const l of leases) map[l.serial] = l.task_id;
        setLeasedSerials(map);
      } catch {
        // control API not ready yet / transient — ignore, next tick retries
      }
    };
    poll();
    const timer = setInterval(poll, intervalMs);
    return () => {
      alive = false;
      clearInterval(timer);
    };
  }, [intervalMs, setLeasedSerials]);
}
