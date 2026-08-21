# 标题带更新入口与确认安装

日期：2026-08-21

## 决策

桌面端不恢复 About/版本页面，也不保留仅为该页面服务的版本信息 IPC。更新入口继续属于出树 `dsh-desktop-bridge` 插件：macOS 将它嵌在左上角标题带 rail，顺序固定为侧栏开关、条件更新按钮、仅收起态可见的新会话 `+`；没有融合标题栏的平台保留右上角 fallback。

后台检查保持低打扰：插件挂载 3 秒后首次检查，之后每 2 小时强制刷新。离线、端点缺失、无更新都不展示入口。发现新版本后展示更新控件并**自动后台下载**（同版本每会话一次；详见 `2026-08-21-updater-auto-download-progress.md`），下载与校验期间同一位置显示旋转图标与底部进度条。校验完成后只亮已下载图标，确认框按点击打开，避免后台下载结束时打断当前会话。

下载与安装拆成两个 IPC：

- `dsh_desktop_download_update` 重新检查目标、流式下载并完成 Tauri 签名校验，将 `Update + Vec<u8>` 仅暂存在当前 Rust 进程，状态进入 `ready`。
- `dsh_desktop_install_update` 只接受 `ready`，消费暂存包完成安装并重启。

`Update::download` 的完成回调早于签名验证，因此不能在回调中发布 `ready`；只有 `download().await` 成功返回后才进入 `ready`。确认框只在用户点击已下载图标后打开。用户选择“稍后”只关闭弹窗，不丢弃已验证包；再次点击 check 图标可重新打开确认框。下载成功本身不代表用户同意安装。

## 状态与并发

Rust 是进程级状态的唯一事实源：`idle/checking/current/available/preparing/downloading/ready/installing/restarting/failed`。浏览器 coordinator 串行化 check/download/install，并以 generation 丢弃迟到响应；后台错误保持静默，显式下载失败保留可重试入口。安装成功必须重启，因此浏览器侧 `installUpdate()` 的契约是 `Promise<never>`。

## UI 锚点

rail 的新会话显隐必须使用 `[data-desktop-new-session]`，不得再依赖 `button:nth-child(2)`；插入更新按钮后，位置选择器会误控制更新按钮。Modal 经 portal 渲染，不受 rail 内按钮 reset 样式影响。

## 验收边界

单元测试覆盖状态解码、可见性、coordinator 串行化、Rust claim 转移和下载字节累计；React 组件测试覆盖自动下载、下载完成不自动弹窗/不自动安装、确认框按点击打开、“稍后”保留、失败不循环以及再次打开后明确安装。桌面 e2e 仍需验证按钮与灯排同线、顺序、收起态 `+`、旋转态以及没有 About 导航项。没有高于当前版本的真实签名 Release 时，不执行真实安装；最终签名校验和重启链留给下一次发布候选包验证。
