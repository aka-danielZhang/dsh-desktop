// @vitest-environment jsdom
import { cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
import { afterEach, expect, test, vi } from 'vitest'
import { UpdateControl } from '../src/client/update-indicator.tsx'
import { en, type DesktopBridgeKey } from '../src/client/locales.ts'
import type { DesktopUpdateStatus } from '../src/client/updates.ts'

afterEach(() => { cleanup() })

const t = (key: DesktopBridgeKey, params?: Record<string, unknown>): string => {
  let text = en[key]
  for (const [name, value] of Object.entries(params ?? {})) text = text.replaceAll(`{${name}}`, String(value))
  return text
}

function deferred<T>(): { promise: Promise<T>; resolve(value: T): void; reject(error: unknown): void } {
  let resolve!: (value: T) => void
  let reject!: (error: unknown) => void
  const promise = new Promise<T>((accept, decline) => {
    resolve = accept
    reject = decline
  })
  return { promise, resolve, reject }
}

test('available updates auto-download; install waits for an explicit click', async () => {
  const initial = deferred<unknown>()
  const install = deferred<never>()
  let generation = 0
  let snapshot: DesktopUpdateStatus = { phase: 'available', version: '0.3.0', notes: '' }
  const update = {
    checkUpdate: vi.fn(() => initial.promise),
    getUpdateStatus: vi.fn(async (): Promise<DesktopUpdateStatus> => snapshot),
    updateGeneration: () => generation,
    downloadUpdate: vi.fn(async () => {
      generation += 1
      snapshot = { phase: 'ready', version: '0.3.0' }
    }),
    installUpdate: vi.fn(() => install.promise),
    t,
  }
  render(<UpdateControl {...update} />)

  await waitFor(() => { expect(update.downloadUpdate).toHaveBeenCalledTimes(1) })
  const readyTitle = en['update.ready'].replace('{version}', '0.3.0')
  await waitFor(() => { expect(screen.getByRole('button', { name: readyTitle })).toBeTruthy() })
  expect(screen.queryByRole('dialog')).toBeNull()
  expect(update.installUpdate).not.toHaveBeenCalled()

  fireEvent.click(screen.getByRole('button', { name: readyTitle }))
  const dialog = screen.getByRole('dialog', { name: en['update.confirm.title'] })
  fireEvent.click(within(dialog).getByText(en['update.confirm.later']))
  expect(screen.queryByRole('dialog')).toBeNull()

  fireEvent.click(screen.getByRole('button', { name: readyTitle }))
  const reopened = screen.getByRole('dialog', { name: en['update.confirm.title'] })
  fireEvent.click(within(reopened).getByText(en['update.confirm.install']))
  expect(update.installUpdate).toHaveBeenCalledTimes(1)
  expect(screen.queryByRole('dialog')).toBeNull()
})

test('auto-download runs once per version; available rebound stays click-to-retry', async () => {
  const initial = deferred<unknown>()
  let generation = 0
  let snapshot: DesktopUpdateStatus = { phase: 'available', version: '0.3.0', notes: '' }
  const update = {
    checkUpdate: vi.fn(() => initial.promise),
    getUpdateStatus: vi.fn(async (): Promise<DesktopUpdateStatus> => snapshot),
    updateGeneration: () => generation,
    downloadUpdate: vi.fn(async () => {
      generation += 1
      throw new Error('network')
    }),
    installUpdate: vi.fn(async () => {
      throw new Error('unreachable')
    }),
    t,
  }
  render(<UpdateControl {...update} />)

  await waitFor(() => { expect(update.downloadUpdate).toHaveBeenCalledTimes(1) })
  const availableTitle = en['update.available'].replace('{version}', '0.3.0')
  await waitFor(() => { expect(screen.getByRole('button', { name: availableTitle })).toBeTruthy() })
  expect(update.downloadUpdate).toHaveBeenCalledTimes(1)
  expect(screen.queryByRole('dialog')).toBeNull()

  fireEvent.click(screen.getByRole('button', { name: availableTitle }))
  await waitFor(() => { expect(update.downloadUpdate).toHaveBeenCalledTimes(2) })
  expect(update.installUpdate).not.toHaveBeenCalled()
})

test('failed auto-download keeps a retry entry that rechecks then downloads', async () => {
  const initial = deferred<unknown>()
  let generation = 0
  let snapshot: DesktopUpdateStatus = { phase: 'available', version: '0.3.0', notes: '' }
  let downloads = 0
  const update = {
    checkUpdate: vi.fn((force?: boolean) => force === true
      ? Promise.resolve({ version: '0.3.0', notes: '' })
      : initial.promise),
    getUpdateStatus: vi.fn(async (): Promise<DesktopUpdateStatus> => {
      if (snapshot.phase === 'failed') throw new Error('status unavailable')
      return snapshot
    }),
    updateGeneration: () => generation,
    downloadUpdate: vi.fn(async () => {
      downloads += 1
      generation += 1
      if (downloads === 1) {
        snapshot = { phase: 'failed', version: '0.3.0', message: 'network' }
        throw new Error('network')
      }
      snapshot = { phase: 'ready', version: '0.3.0' }
    }),
    installUpdate: vi.fn(async () => {
      throw new Error('unreachable')
    }),
    t,
  }
  render(<UpdateControl {...update} />)

  await waitFor(() => { expect(screen.getByRole('button', { name: en['update.failed'] })).toBeTruthy() })
  expect(update.downloadUpdate).toHaveBeenCalledTimes(1)

  fireEvent.click(screen.getByRole('button', { name: en['update.failed'] }))
  await waitFor(() => { expect(update.checkUpdate).toHaveBeenCalledWith(true) })
  const readyTitle = en['update.ready'].replace('{version}', '0.3.0')
  await waitFor(() => { expect(screen.getByRole('button', { name: readyTitle })).toBeTruthy() })
  expect(update.downloadUpdate).toHaveBeenCalledTimes(2)
  expect(screen.queryByRole('dialog')).toBeNull()
  expect(update.installUpdate).not.toHaveBeenCalled()
})

test('download progress renders a determinate bar from status bytes', async () => {
  const initial = deferred<unknown>()
  let generation = 0
  let snapshot: DesktopUpdateStatus = {
    phase: 'downloading',
    version: '0.3.0',
    downloaded: 25,
    total: 100,
  }
  const update = {
    checkUpdate: vi.fn(() => initial.promise),
    getUpdateStatus: vi.fn(async (): Promise<DesktopUpdateStatus> => snapshot),
    updateGeneration: () => generation,
    downloadUpdate: vi.fn(async () => undefined),
    installUpdate: vi.fn(async () => {
      throw new Error('unreachable')
    }),
    t,
  }
  render(<UpdateControl {...update} />)

  const progressTitle = en['update.progress'].replace('{percent}', '25')
  await waitFor(() => { expect(screen.getByRole('button', { name: progressTitle })).toBeTruthy() })
  const button = screen.getByRole('button', { name: progressTitle })
  expect(button.style.width).toBe('22px')
  const bar = within(button).getByRole('progressbar')
  expect(bar.getAttribute('aria-valuenow')).toBe('25')
  const fill = bar.querySelector('span') as HTMLElement | null
  expect(fill?.style.width).toBe('25%')
  expect(update.downloadUpdate).not.toHaveBeenCalled()
})
