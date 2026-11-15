import {useCallback, useEffect, useState} from 'react';
import {Button, run} from './primitives';
import {useStartupCopy} from './StartupGuide';
import './update-notices.css';

// In-app updater UI (Windows shell `appUpdate` bridge). Status is pushed by
// the shell as `nuphylab:app-update` CustomEvents and pulled once on mount;
// browser previews (no bridge) render nothing.

type AppUpdateNotes = {version: string; zh: string; en: string};
type AppUpdateStatus = {
  phase: 'idle' | 'checking' | 'available' | 'downloading' | 'verified' | 'installing' | 'error';
  upToDate: boolean;
  currentVersion: string;
  version: string | null;
  progress: number | null;
  error: {zh: string; en: string} | null;
  notes: AppUpdateNotes | null;
  autoCheck: boolean;
};

async function nativeUpdate(op: string, body: Record<string, unknown> = {}): Promise<AppUpdateStatus> {
  const bridge = (window as unknown as {
    webkit?: {messageHandlers?: {appUpdate?: {postMessage: (body: unknown) => Promise<unknown>}}};
  }).webkit?.messageHandlers?.appUpdate;
  if (!bridge) throw new Error('unavailable');
  return (await bridge.postMessage({op, ...body})) as AppUpdateStatus;
}

export function AppUpdatePreferences() {
  const copy = useStartupCopy();
  const [status, setStatus] = useState<AppUpdateStatus | null>(null);
  const [available, setAvailable] = useState(false);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    const bridge = (window as unknown as {
      webkit?: {messageHandlers?: {appUpdate?: {postMessage: (body: unknown) => Promise<unknown>}}};
    }).webkit?.messageHandlers?.appUpdate;
    if (!bridge) return;
    setAvailable(true);
    let active = true;
    nativeUpdate('status').then(value => { if (active) setStatus(value); }).catch(() => {});
    const onChange = (event: Event) => {
      const detail = (event as CustomEvent<AppUpdateStatus>).detail;
      if (active && detail) setStatus(detail);
    };
    window.addEventListener('nuphylab:app-update', onChange);
    return () => { active = false; window.removeEventListener('nuphylab:app-update', onChange); };
  }, []);

  const check = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    try {
      setStatus(await nativeUpdate('check'));
    } catch {
      /* the pushed events keep the row current; a failed check surfaces there */
    } finally {
      setBusy(false);
    }
  }, [busy]);

  const install = useCallback(async () => {
    if (busy) return;
    setBusy(true);
    try {
      setStatus(await nativeUpdate('install'));
    } catch {
      /* status events carry the failure reason */
    } finally {
      setBusy(false);
    }
  }, [busy]);

  if (!available) return null;
  const phase = status?.phase || 'idle';
  const errorMessage = status?.error ? copy(status.error.zh, status.error.en) : '';
  const notes = status?.notes;
  const statusLine = phase === 'checking' ? copy('正在检查更新…', 'Checking for updates…')
    : phase === 'available' ? copy(`发现新版本 ${status?.version || ''}。`, `New version ${status?.version || ''} is available.`)
    : phase === 'downloading' ? copy(`正在下载更新 ${status?.progress ?? 0}%…`, `Downloading the update ${status?.progress ?? 0}%…`)
    : phase === 'verified' ? copy('校验通过，正在准备安装。', 'Verified. Preparing to install.')
    : phase === 'installing' ? copy('正在退出并安装更新，键盘灯效会先恢复。', 'Quitting to install the update. The keyboard lighting is restored first.')
    : phase === 'error' ? errorMessage
    : status?.upToDate ? copy('当前已是最新版本。', "You're up to date.")
    : copy('可在此检查是否有新版本。', 'Check here for new versions.');
  return <section className="lab-setting-panel lab-update-panel" aria-label={copy('应用更新', 'App update')}>
    <div className="lab-setting-row">
      <div>
        <h3>{copy('应用更新', 'App update')}</h3>
        <p role="status">{statusLine}</p>
        {phase === 'available' && notes && (notes.zh || notes.en)
          ? <p className="lab-update-notes">{copy(notes.zh, notes.en)}</p>
          : null}
      </div>
      {phase === 'available'
        ? <Button kind="primary" busy={busy} onClick={() => run(install())}>{copy('下载并安装', 'Download and install')}</Button>
        : <Button busy={busy || phase === 'checking'} disabled={phase === 'downloading' || phase === 'installing' || phase === 'verified'} onClick={() => run(check())}>{copy('检查更新', 'Check for updates')}</Button>}
    </div>
    {status?.autoCheck ? null : <p className="settings-v6-note">{copy('启动后的自动检查已关闭，可在需要时手动检查。', 'The automatic check after launch is off; check manually when needed.')}</p>}
  </section>;
}
