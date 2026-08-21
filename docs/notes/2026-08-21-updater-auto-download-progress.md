# 更新：后台自动下载与进度条

日期：2026-08-21

## 决策

在既有「检查 / 下载 / 确认安装」拆分之上，把**下载**从用户点击改成发现新版后的后台自动行为；**安装**仍必须显式确认。进度从仅 tooltip 百分比升级为控件底部可见进度条。

## 理由

- 静默轮询（3s 首查 / 2h 周期）已经接受「发现新版」的打扰阈值；再多一步「点下载」只会拉长到可安装的时间，且大包在用户主动点之前白白浪费窗口期。
- Zed / GitHub Desktop 的共识是：后台下载 + 有更新才出现 affordance + 安装需确认。本仓已有确认框，缺的是自动下载与可视进度。
- 自动下载失败必须可点重试；同版本每挂载会话只自动一次，避免失败态与 `available` 回流形成死循环。

## 行为

1. `checkUpdate` → 状态 `available` → 插件立即 `downloadUpdate()`（不经点击）。
2. `preparing` / `downloading`：控件略加宽，底部 2px 进度条；有 `total` 填百分比，否则不定长滑动；`prefers-reduced-motion` 关掉动画。
3. `ready`：弹确认框；「稍后」只关窗，已校验包保留；再点 check 图标重开确认。
4. 失败：保留失败入口，点击重试（先强制 recheck）。

## 不变契约

IPC 表与 Rust 进程级状态机不变。浏览器 coordinator 仍串行化 check/download/install；generation 丢弃迟到响应。安装成功必须重启（`installUpdate(): Promise<never>`）。

## 验收

- 组件测试：进入 `available` 即调用 `downloadUpdate`；下载完成不自动 `installUpdate`；进度态渲染 `aria-valuenow` 与条宽。
- 既有 coordinator / 状态解码单测保持。
- 决策与 AGENTS「标题带更新入口」同 PR 更新。
