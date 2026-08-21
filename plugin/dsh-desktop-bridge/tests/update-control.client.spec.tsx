// @vitest-environment jsdom
import { act, cleanup, fireEvent, render, screen, waitFor, within } from '@testing-library/react'
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

test('available updates auto-download and still require install confirmation', async () => {
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
  await act(async () => { await new Promise(resolve => setTimeout(resolve, 140)) })

  const dialog = screen.getByRole('dialog', { name: en['update.confirm.title'] })
  expect(update.installUpdate).not.toHaveBeenCalled()
  fireEvent.click(within(dialog).getByText(en['update.confirm.later']))
  expect(screen.queryByRole('dialog')).toBeNull()

  const readyTitle = en['update.ready'].replace('{version}', '0.3.0')
  fireEvent.click(screen.getByRole('button', { name: readyTitle }))
  const reopened = screen.getByRole('dialog', { name: en['update.confirm.title'] })
  fireEvent.click(within(reopened).getByText(en['update.confirm.install']))
  expect(update.installUpdate).toHaveBeenCalledTimes(1)
  expect(screen.queryByRole('dialog')).toBeNull()
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
  expect(button.getAttribute('aria-valuenow')).toBe('25')
  const fill = button.querySelector('[data-desktop-update-progress] > span') as HTMLElement | null
  expect(fill?.style.width).toBe('25%')
})
