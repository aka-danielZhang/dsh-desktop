/** Quiet periodic updater control shared by the rail and non-mac fallback. */
import { useCallback, useEffect, useRef, useState, type ReactElement } from 'react'
import {
  Button,
  IconCheckOutline16,
  IconDownloadOutline16,
  IconLoadingOutline16,
  Modal,
} from '@deepseek-ai/dsh-client-ui-primitives'
import type { PropsLocale } from '@deepseek-ai/dsh-client-ui-slots'
import {
  isUpdateBusy, isUpdateIndicatorVisible, statusFromCheck, updatePercent,
  type DesktopUpdaterInjected, type DesktopUpdateStatus,
} from './updates.ts'

export type UpdateIndicatorInjected = DesktopUpdaterInjected
export type UpdateIndicatorProps = UpdateIndicatorInjected & PropsLocale<'desktop-bridge'>

/** Periodic check interval (quiet background poll; 2h). */
const UPDATE_INTERVAL_MS = 2 * 60 * 60 * 1000
/** First check delay after mount, beyond the boot request burst. */
const FIRST_CHECK_DELAY_MS = 3000

/** Shared CSS for the spinner and indeterminate progress track. */
const UPDATE_CONTROL_CSS = [
  '@keyframes desktop-update-spin{to{transform:rotate(360deg)}}',
  '@keyframes desktop-update-indeterminate{0%{transform:translateX(-100%)}100%{transform:translateX(250%)}}',
  '[data-desktop-update-spinner]{display:inline-flex;animation:desktop-update-spin .8s linear infinite}',
  '[data-desktop-update-progress]{position:absolute;left:2px;right:2px;bottom:1px;height:2px;overflow:hidden;border-radius:1px;background:color-mix(in srgb,var(--dsw-alias-label-primary) 18%,transparent);pointer-events:none}',
  '[data-desktop-update-progress]>span{display:block;height:100%;border-radius:inherit;background:var(--dsw-alias-label-primary)}',
  '[data-desktop-update-progress][data-indeterminate=""]>span{width:40%;animation:desktop-update-indeterminate 1.1s ease-in-out infinite}',
  '@media (prefers-reduced-motion:reduce){[data-desktop-update-spinner],[data-desktop-update-progress][data-indeterminate=""]>span{animation:none}[data-desktop-update-progress][data-indeterminate=""]>span{width:100%}}',
].join('')

/** The compact updater button rendered beside the sidebar toggle. */
export function UpdateControl(props: UpdateIndicatorProps): ReactElement | null {
  const { checkUpdate, getUpdateStatus, updateGeneration, downloadUpdate, installUpdate, t } = props
  const [status, setStatus] = useState<DesktopUpdateStatus>({ phase: 'idle' })
  const [requested, setRequested] = useState(false)
  const [confirmOpen, setConfirmOpen] = useState(false)
  const mounted = useRef(true)
  const statusRequest = useRef(0)
  /** One auto-download attempt per version per mount; failures stay click-to-retry. */
  const autoDownloadVersion = useRef<string | undefined>()

  const refreshStatus = useCallback(async (
    requestGeneration: number,
    fallback?: DesktopUpdateStatus,
  ): Promise<void> => {
    const sequence = ++statusRequest.current
    try {
      const snapshot = await getUpdateStatus()
      if (mounted.current && updateGeneration() === requestGeneration && statusRequest.current === sequence) {
        setStatus(snapshot)
      }
    } catch {
      if (fallback !== undefined
        && mounted.current
        && updateGeneration() === requestGeneration
        && statusRequest.current === sequence) {
        setStatus(fallback)
      }
    }
  }, [getUpdateStatus, updateGeneration])

  const startDownload = useCallback((target?: string): void => {
    setRequested(true)
    setStatus(target === undefined ? { phase: 'preparing' } : { phase: 'preparing', version: target })
    void (async () => {
      try {
        const request = downloadUpdate()
        const requestGeneration = updateGeneration()
        await request
        await refreshStatus(requestGeneration)
      } catch {
        const fallback: DesktopUpdateStatus = target === undefined
          ? { phase: 'failed', message: 'Update download failed' }
          : { phase: 'failed', version: target, message: 'Update download failed' }
        await refreshStatus(updateGeneration(), fallback)
      }
    })()
  }, [downloadUpdate, refreshStatus, updateGeneration])

  useEffect(() => {
    mounted.current = true
    const run = (force: boolean): void => {
      const request = checkUpdate(force)
      const requestGeneration = updateGeneration()
      request.then(
        (found) => {
          if (mounted.current && updateGeneration() === requestGeneration) setRequested(false)
          void refreshStatus(requestGeneration, statusFromCheck(found))
        },
        () => { void refreshStatus(requestGeneration) },
      )
    }
    void refreshStatus(updateGeneration())
    const first = setTimeout(() => { run(false) }, FIRST_CHECK_DELAY_MS)
    const interval = setInterval(() => { run(true) }, UPDATE_INTERVAL_MS)
    return () => {
      mounted.current = false
      clearTimeout(first)
      clearInterval(interval)
    }
  }, [checkUpdate, refreshStatus, updateGeneration])

  useEffect(() => {
    if (!isUpdateBusy(status)) return
    let pending = false
    const poll = (): void => {
      if (pending) return
      pending = true
      const requestGeneration = updateGeneration()
      refreshStatus(requestGeneration).finally(() => { pending = false })
    }
    const timer = setInterval(poll, 120)
    return () => { clearInterval(timer) }
  }, [refreshStatus, status, updateGeneration])

  const availableVersion = status.phase === 'available' ? status.version : undefined
  // Discover → background download. Ready stays quiet until the user clicks.
  useEffect(() => {
    if (availableVersion === undefined) return
    if (autoDownloadVersion.current === availableVersion) return
    autoDownloadVersion.current = availableVersion
    startDownload(availableVersion)
  }, [availableVersion, startDownload])

  const onDownload = useCallback(() => {
    if (status.phase === 'ready') {
      setConfirmOpen(true)
      return
    }
    if (isUpdateBusy(status)) return
    const target = 'version' in status ? status.version : undefined
    void (async () => {
      try {
        if (status.phase === 'failed') {
          const found = await checkUpdate(true)
          if (found === null) {
            setRequested(false)
            setStatus({ phase: 'current' })
            await refreshStatus(updateGeneration(), { phase: 'current' })
            return
          }
          autoDownloadVersion.current = found.version
          startDownload(found.version)
          return
        }
        if (target !== undefined) autoDownloadVersion.current = target
        startDownload(target)
      } catch {
        const fallback: DesktopUpdateStatus = target === undefined
          ? { phase: 'failed', message: 'Update download failed' }
          : { phase: 'failed', version: target, message: 'Update download failed' }
        await refreshStatus(updateGeneration(), fallback)
      }
    })()
  }, [checkUpdate, refreshStatus, startDownload, status, updateGeneration])

  const onInstall = useCallback(() => {
    if (status.phase !== 'ready') return
    setConfirmOpen(false)
    const request = installUpdate()
    const requestGeneration = updateGeneration()
    setStatus({ phase: 'installing', version: status.version })
    request.catch(() => {
      void refreshStatus(requestGeneration, {
        phase: 'failed',
        version: status.version,
        message: 'Update install failed',
      })
    })
  }, [installUpdate, refreshStatus, status, updateGeneration])

  const visible = isUpdateIndicatorVisible(status) || (requested && status.phase === 'failed')
  if (!visible) return null

  const busy = isUpdateBusy(status)
  const percent = updatePercent(status)
  const downloading = status.phase === 'downloading' || status.phase === 'preparing'
  const title = status.phase === 'available'
    ? t('update.available', { version: status.version })
    : status.phase === 'downloading' && percent !== undefined
      ? t('update.progress', { percent })
      : status.phase === 'ready'
        ? t('update.ready', { version: status.version })
        : status.phase === 'failed'
          ? t('update.failed')
          : status.phase === 'installing' || status.phase === 'restarting'
            ? t('update.installing')
            : t('update.preparing')
  const icon = status.phase === 'ready'
    ? <IconCheckOutline16 />
    : busy
      ? <span data-desktop-update-spinner=""><IconLoadingOutline16 /></span>
      : <IconDownloadOutline16 />

  return (
    <>
      <style>{UPDATE_CONTROL_CSS}</style>
      <button
        type="button"
        data-desktop-rail-button=""
        data-desktop-update-button=""
        aria-label={title}
        aria-busy={busy}
        title={title}
        onClick={onDownload}
        disabled={busy}
        style={{
          all: 'unset',
          boxSizing: 'border-box',
          position: 'relative',
          display: 'inline-flex',
          alignItems: 'center',
          justifyContent: 'center',
          width: '22px',
          height: '22px',
          borderRadius: '6px',
          cursor: busy ? 'default' : 'pointer',
          opacity: busy ? 0.92 : 1,
          color: 'inherit',
          pointerEvents: 'auto',
        }}
        onMouseEnter={(event) => { if (!busy) event.currentTarget.style.background = 'var(--dsw-alias-interactive-bg-hover)' }}
        onMouseLeave={(event) => { event.currentTarget.style.background = 'transparent' }}
      >
        {icon}
        {downloading ? (
          <span
            data-desktop-update-progress=""
            {...(percent === undefined ? { 'data-indeterminate': '' } : {})}
            role="progressbar"
            aria-label={title}
            aria-valuemin={0}
            aria-valuemax={100}
            aria-valuenow={percent}
          >
            <span style={percent === undefined ? undefined : { width: `${percent}%` }} />
          </span>
        ) : null}
      </button>
      <Modal
        open={confirmOpen && status.phase === 'ready'}
        onClose={() => { setConfirmOpen(false) }}
        title={t('update.confirm.title')}
        closeLabel={t('update.confirm.later')}
        description={status.phase === 'ready' ? t('update.confirm.description', { version: status.version }) : ''}
        footer={(
          <>
            <Button variant="outline" size="sm" onClick={() => { setConfirmOpen(false) }}>
              {t('update.confirm.later')}
            </Button>
            <Button variant="primary" size="sm" onClick={onInstall}>
              {t('update.confirm.install')}
            </Button>
          </>
        )}
      />
    </>
  )
}

/** Non-macOS fallback where no overlay-titlebar rail exists. */
export function UpdateIndicator(props: UpdateIndicatorProps): ReactElement {
  return (
    <div
      data-desktop-update-indicator=""
      style={{
        position: 'absolute',
        top: '8px',
        right: '14px',
        height: '22px',
        display: 'flex',
        alignItems: 'center',
        zIndex: 1,
        color: 'var(--dsw-alias-label-primary)',
        pointerEvents: 'none',
      }}
    >
      <UpdateControl {...props} />
    </div>
  )
}
