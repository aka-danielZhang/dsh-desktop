# 更新：后台自动下载与进度条

日期：2026-08-21

## 决策

在既有「检查 / 下载 / 确认安装」拆分之上，把**下载**从用户点击改成发现新版后的后台自动行为；**安装**仍必须显式确认，且确认框只在用户点击已下载图标后打开。进度从仅 tooltip 百分比升级为控件底部可见进度条，控件宽度保持 22px，不挤标题带。

## 理由

- 静默轮询（3s 首查 / 2h 周期）已经接受「发现新版」的打扰阈值；再多一步「点下载」只会拉长到可安装的时间，且大包在用户主动点之前白白浪费窗口期。
- Zed / GitHub Desktop 的共识是：后台下载 + 有更新才出现 affordance + 安装需确认。自动下载完成后若立刻弹出「安装并重启」，会把低打扰的后台检查做成打断当前会话的强制决策，和「下载 ≠ 授权安装」打架。ready 态只亮 check 图标；Modal 走点击。
- 自动下载失败必须可点重试；同版本每挂载会话只自动一次，避免失败态与 `available` 回流形成死循环。
- `available` 文案保持动作句（「下载 v{version}」）。该态既是自动下载前的短暂闪现，也是失败后回流、需要用户再点的入口；不能写成「正在后台下载」。

## 行为

1. `checkUpdate` → 状态 `available` → 插件立即 `downloadUpdate()`（不经点击）。
2. `preparing` / `downloading`：22px 控件底部 2px 进度条；有 `total` 填百分比，否则不定长滑动；`prefers-reduced-motion` 关掉动画并把不定长条铺满，避免看起来像卡在 40%。
3. `ready`：控件变为已下载图标，**不自动弹窗**。点击才开确认框；「稍后」只关窗，已校验包保留；再点 check 图标重开确认。
4. 失败：保留失败入口，点击重试（先强制 recheck）。同版本回流到 `available` 也不再自动下载。

## 不变契约

IPC 表与 Rust 进程级状态机不变。浏览器 coordinator 仍串行化 check/download/install；generation 丢弃迟到响应。安装成功必须重启（`installUpdate(): Promise<never>`）。

## 验收

- 组件测试：进入 `available` 即调用 `downloadUpdate`；下载完成不自动打开确认框、不自动 `installUpdate`；点击已下载图标才开确认框。
- 同版本自动下载只一次；`available` 回流不循环；失败入口点击后先 recheck 再下载。
- 进度态在 22px 控件内渲染 `progressbar` 的 `aria-valuenow` 与条宽。
- 既有 coordinator / 状态解码单测保持。
- 决策与 AGENTS「标题带更新入口」同 PR 更新。
