/** `desktop-bridge` namespace dictionaries. */

/** Simplified Chinese dictionary (the key-set source of truth). */
export const zh = {
  'badge.text': 'web端',
  'badge.openBrowser': '在系统浏览器打开 web 端',
  'rail.toggle': '切换侧边栏',
  'rail.newSession': '新会话',
  'update.available': '发现更新 v{version}，正在后台下载',
  'update.progress': '正在下载更新：{percent}%',
  'update.preparing': '正在准备更新',
  'update.ready': 'v{version} 已下载',
  'update.failed': '更新下载失败，点击重试',
  'update.installing': '正在安装更新，完成后自动重启',
  'update.confirm.title': '更新已下载',
  'update.confirm.description': 'v{version} 已下载并完成签名验证。现在安装并重启吗？',
  'update.confirm.later': '稍后',
  'update.confirm.install': '安装并重启',
} satisfies Record<string, string>

/** The namespace key union. */
export type DesktopBridgeKey = keyof typeof zh

/** English dictionary, checked complete against the zh key set. */
export const en = {
  'badge.text': 'Web',
  'badge.openBrowser': 'Open the web end in the system browser',
  'rail.toggle': 'Toggle sidebar',
  'rail.newSession': 'New session',
  'update.available': 'Update v{version} found; downloading in background',
  'update.progress': 'Downloading update: {percent}%',
  'update.preparing': 'Preparing update',
  'update.ready': 'v{version} downloaded',
  'update.failed': 'Update download failed; click to retry',
  'update.installing': 'Installing update; the app will restart',
  'update.confirm.title': 'Update downloaded',
  'update.confirm.description': 'v{version} is downloaded and signature-verified. Install it and restart now?',
  'update.confirm.later': 'Later',
  'update.confirm.install': 'Install and restart',
} satisfies Record<DesktopBridgeKey, string>

/** The locale namespace id this plugin registers. */
export const NS = 'desktop-bridge'
