import { getVersion } from '@tauri-apps/api/app';
import { invoke } from '@tauri-apps/api/core';
import { listen, type UnlistenFn } from '@tauri-apps/api/event';
import type {
  AssessmentReport,
  AssessmentState,
  NatTaskInput,
  P2pTaskInput,
  ProbeError,
  ReportSummary,
} from '../types/protocol';

export function startNatAssessment(input: NatTaskInput): Promise<string> {
  return invoke<string>('start_nat_assessment', { input });
}

export function startP2pAssessment(input: P2pTaskInput): Promise<string> {
  return invoke<string>('start_p2p_assessment', { input });
}

export function cancelCurrent(reason: string | null = null): Promise<void> {
  return invoke<void>('cancel_current', { reason });
}

export function listHistory(): Promise<ReportSummary[]> {
  return invoke<ReportSummary[]>('list_history');
}

export function getReport(id: string): Promise<AssessmentReport> {
  return invoke<AssessmentReport>('get_report', { id });
}

export function deleteReport(id: string): Promise<void> {
  return invoke<void>('delete_report', { id });
}

export function getCurrentState(): Promise<AssessmentState> {
  return invoke<AssessmentState>('get_current_state');
}

export async function getAppVersion(fallback: string): Promise<string> {
  try {
    return await getVersion();
  } catch {
    return fallback;
  }
}

export async function openExternalUrl(url: string): Promise<void> {
  try {
    const handled = await invoke<boolean>('open_external_url', { url });
    if (handled) return;
  } catch {
    // Fall back to anchor click on Android / browser runtimes
  }
  if (typeof document !== 'undefined') {
    const anchor = document.createElement('a');
    anchor.href = url;
    anchor.target = '_blank';
    anchor.rel = 'noopener noreferrer';
    document.body.appendChild(anchor);
    anchor.click();
    anchor.remove();
  }
}

export function subscribeAssessmentState(
  handler: (state: AssessmentState) => void,
): Promise<UnlistenFn> {
  return listen<AssessmentState>('assessment://state', (event) => {
    handler(event.payload);
  });
}

export function parseInvokeError(err: unknown): ProbeError {
  if (typeof err === 'string') {
    if (
      err === 'unauthorized' ||
      err === 'room_not_found' ||
      err === 'room_full' ||
      err === 'peer_timeout' ||
      err === 'peer_left' ||
      err === 'cancelled'
    ) {
      return err;
    }
    return { internal: err };
  }
  if (err && typeof err === 'object') {
    const obj = err as Record<string, unknown>;
    for (const key of [
      'invalid_input',
      'server_unreachable',
      'punch_failed',
      'internal',
    ] as const) {
      if (typeof obj[key] === 'string') {
        return { [key]: obj[key] } as ProbeError;
      }
    }
    if (typeof obj.message === 'string') {
      return { internal: obj.message };
    }
  }
  return { internal: String(err) };
}
