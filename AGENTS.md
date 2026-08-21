# AGENTS.md

dsh-desktop 是 [DeepSeek Harness](https://github.com/deepseek-ai/deepseek-harness)（下称 DSH）的桌面化 monorepo：出树插件与 Tauri 壳同仓、独立发版。两个平面：

- **`plugin/<name>/`** —— 可独立安装、独立打 tag 的 DSH 插件包。成员：`dsh-desktop-bridge`（桌面门控：外链路由、原生注意力通知、桌面指示）、`dsh-mcp-settings`（2026-08-19 subtree 迁入）、`dsh-provider-balance`（2026-08-19 subtree 迁入，纯 DOM 注入）、`dsh-reasoning-efforts`（2026-08-20 新写，host-only：给手写 llm-pi-ai 模型补 `reasoningEfforts` 声明，契约见包内 README，决策见 `docs/notes/2026-08-20-reasoning-efforts.md`）、`dsh-web-search-toggle`（2026-08-20 新写，双面：通用设置页「Web Search」开关——DEEPSEEK_API_KEY 状态提示 + home patch 层受管块禁用 tool-web 行，契约见 `docs/notes/2026-08-20-web-search-toggle.md`）、`dsh-compaction-hierarchical`（2026-08-20 新写，host-only：继承 stock compaction 事务，以有界 map-reduce 让小上下文模型压缩大历史；契约见包内 README，决策见 `docs/notes/2026-08-20-hierarchical-compaction.md`）、`dsh-branding`（2026-08-21 新写，browser-only：占用 `sidebar.brand.name` 替换字标为 "Oh My DSH"+"Harness" pill 并重写 document title——**始终挂载、无桌面门控**，终端/浏览器/桌面同一条路；决策见 `docs/notes/2026-08-21-branding-plugin.md`）。
- **Tauri 2 Rust 壳** —— spawn harness sidecar、端口分配、就绪检测、窗口加载。壳层不含业务逻辑；harness 不感知壳的存在。业务集成只特殊对待桥插件（gate + IPC）；分发层另按本文件的 desktop-owned 清单打包/安装层次压缩，但不读取其 Provider 业务。

规范层级：[README.md](README.md) 记录「为什么」（技术选型）；本文件记录「契约与约定」（怎么做）；代码是实现。冲突时以本文件为准。改契约必须同 PR 改本文件。

## Repository layout

```
plugin/<name>/               一个可独立发版的 DSH 插件包（目录名 === package.json name）
  dsh-desktop-bridge/        桌面门控桥 + 日志汇（本文件「插件契约」一节）
    src/index.ts             host half：surface 插件，空 apply
    src/log-sink.ts          日志汇 host 行：ctx.logger → 每启动一个 JSONL 文件（见「日志汇行」）
    src/invariant.ts         伙伴不变量说明
    src/client/              browser half：env 探测 + 三个桥 + shell.overlay 桌面指示 + 标题栏融合
    tests/                   node:test 单测（纯函数）
src-tauri/                   Tauri 2 壳（不感知插件业务）
scripts/                     壳层与工具脚本：prepare-runtime.mjs、prepare-desktop-bundle.mjs
docs/                        packaging-playbook.md + notes/（决策记录住仓根，不跟包走）
```

`CLAUDE.md` 是指向本文件的 symlink（与 DSH 仓库同惯例）：改 AGENTS.md，不要改链接。

## 插件 monorepo 规范

本仓是「个人 DSH 扩展 + 桌面壳」的单一事实源。插件与桌面同仓，是为了一次 checkout、一次 harness rc bump 过全树，同时保留各自的发布节奏。对照：[dataelement/dsh-desktop](https://github.com/dataelement/dsh-desktop) 把 DSH 钉在 npm 上、用仓根 `patches/` 改上游压缩产物——那是壳侧补丁模型，不是插件布局，不学。

### 落点

- 每个插件一个目录：`plugin/<package.json name>/`。目录名必须等于未加 scope 的包名，因为 `dsh plugin --profile web add <path>` 按这个路径装包，entry id 也是这个名字。
- 一个目录 = 一个可 `plugin add` 的安装单元，自带 `package.json`、`dsh.bundle`/`cordis.patch.yml`、源码、测试。mcp-settings 那种「一包三行」（manager / inventory / ui）仍是**一个**目录、一份 patch，不是三个目录。
- 桥插件不能当容器：它的 apply 在非桌面环境必须零副作用；mcp-settings / provider-balance 在终端 `dsh web` 也要工作。塞进 `dsh-desktop-bridge` 会把「桌面门控」和「始终挂载」搅在一个 fiber 里。
- 不要再套 `plugin/packages/`，不要把插件放到仓根与 `src-tauri/` 平级，不要放进 fork 的 `packages/`。

### 发版

- 插件与桌面**锁步禁止**。各包 `package.json` 的 `version` 独立走动。
- **版本号策略（0.2.0-rc.1 起，学 harness 的 rc 节奏）**：桌面走 semver 预发布段——大功能进 `0.N.0-rc.x`，稳定后摘 `-rc` 出 `0.N.0`，纯修复走 `0.N.M+1`；插件各自 semver，同样允许 `-rc.N`；fork 标识走 `+zw.N` build metadata（semver §10，排序忽略不影响升级链）。**刻意不**在桌面版本里嵌 harness 基线（`0.1.0-rc.7.desktop.1` 这类嵌套段合法但小于已发的 0.1.3，首个新版即断更新链）；基线由 `runtime/revision.json` 记录。**GitHub Release 不勾 prerelease**——`releases/latest` 端点排除 prerelease，勾了 latest.json 即 404、自动更新断链；`-rc` 只体现在版本号语义。release.yml 有防呆：tag 版本 ≠ `tauri.conf.json`/`package.json` 版本即 fail。
- Git tag 无斜杠三分家：桌面 `v<semver>`（例 `v0.2.0-rc.2`，经典风格）；插件 `<包名>-v<semver>`（例 `dsh-provider-balance-v0.4.2`；包名都是 `dsh-*` 起，天然不与 `v*` 冲突，workflow 按「最后一个 `-v`」切名与版本）；**runtime fork 标签 `v<基线>+zw.<补丁>`**（例 `v0.1.0-rc.7+zw.1`——semver build metadata 标识 zw fork，行业标准做法，基线升级时 `+zw.N` 递增；历史 `desktop/v0.1.0/1` 标签仍有效可fetch，revision.json 钉 ref 字符串）。GitHub Release 按 tag 分流，互不覆盖附件。**latest 指针纪律**：桌面自动更新端点 `releases/latest/download/latest.json` 依赖 latest 指针——desktop Release `make_latest: true` 独占，插件 Release 一律 `make_latest: false`（release.yml 已内置；网页手动发插件 Release 时同样不得设为 latest）。
- 安装面保持 `dsh plugin --profile web add <repo>/plugin/<name>`（file: / git 路径均可）。**插件 npm 双通道（2026-08-21 起）**：allowlist 插件（`dsh-mcp-settings`、`dsh-provider-balance`）随 `dsh-*-v*` tag 额外发 npm，安装面多一条裸包名 `dsh plugin --profile web add <name>`（`dsh plugin` 原样转发 pnpm，天然支持 registry 包）；其余插件仍只走 git tag 分发。npm 通道契约：workflow 的 npm channel gate 是唯一 allowlist 事实源；tag 版本已上 npm 则跳过（幂等，重跑安全）；npm 发布在 GitHub Release **之前**，失败即中止（fail loud，不出「tarball 有、npm 无」的半发布态）；token 走 repo secret `NPM_TOKEN`（npm 账号 danielzhang688，与 fork 仓 publish-fork 同一 token）；有 build script 的包（mcp-settings）先按 CI 同款 baseline checkout + 锚 + install + build 再 publish。⚠️ `dsh-provider-balance` 的 npm 包名原由 CalvinQin 注册（其 0.2.0），danielzhang688 无写权限——owner 侧 `npm owner add danielzhang688 dsh-provider-balance` 之前，该包的 npm 步骤会 403 fail loud（属预期，权限补齐后重跑即可）。**对 harness 的依赖**则一律 npm（「npm 依赖纪律」一节），与 fork 的 npm 发布纪律（fork FORK.md）互为两面。
- 壳的 release 只携带其运行面直接依赖的桌面自有插件，不按 `plugin/*` 无差别收包。当前集合是 bridge（`bridge.tar.gz` → `~/.dsh-desktop/bridge/`）与层次压缩（`compaction-hierarchical.tar.gz` → `~/.dsh-desktop/plugins/dsh-compaction-hierarchical/`）：prepare 对两包分别 typecheck/test/build、记录 tarball hash，壳首启原子解压并幂等 `plugin add` 到 web Profile。层次压缩的根 patch 刻意为空，安装只保证用户 preset 可在隔离 `compaction` realm 解析该 Provider，不替用户改 shipped/default preset。新增桌面依赖插件必须同一次变更更新 prepare、Tauri resources、壳安装链和本条清单，并发新 desktop 版本；独立插件 tag 不能替代 desktop Release。

### 迁入既有插件仓

- `git subtree`（或 `--allow-unrelated-histories`）保留历史，禁止拷贝文件了事。
- 源仓工作区必须干净：未提交的发版改动先在源仓落地（mcp-settings 0.2.3 的 credentials 竞态就是这种）。
- 迁入后源仓 archive 为只读，不再双写。
- 迁入当天**不上**仓根 `pnpm-workspace.yaml`：桥锁 pnpm 10，mcp-settings 锁 pnpm 11。各包继续自己的 `pnpm install`；workspace 收敛是独立 PR。
- 迁入当天不统一测试/构建工具链。第二步再把裸 `client.js` 分发（provider-balance）收进桥的 tsdown 纯度门。
- **harness 依赖一律 npm**（「npm 依赖纪律」一节）：插件 devDependencies 钉 registry 版本（`@deepseek-ai/*` 官方包在公共 npm；fork 修改面包的自有 scope 版，见 fork FORK.md「发布纪律」）。**源码 link: 依赖是仅限本地调试的显式 posture**，只能经 `pnpm run link:source` / `unlink:source`（`scripts/source-deps.mjs`）进出，不得提交、不得作为默认形态。不能指望 tsx 套用 checkout 的 tsconfig paths——桌面 runtime 的 tsx 4.23+ 只对 tsconfig include 内的文件生效，bare specifier 走纯 Node 上溯解析（2026-08-19 桌面崩溃循环的根因，见 `docs/notes/2026-08-19-log-sink-race-and-plugin-peer-resolution.md`；这也解释了为何 link posture 仍需包内 node_modules 物化）。

### 跨包纪律

- 跨插件只走 slot 与 ctx 服务，禁止 import 另一插件的实现符号；harness 包只做 type-only import（构建时擦除）。
- 决策记录一律 `docs/notes/`（仓根），不跟包走。包内 README 只写该包的安装与行为。

## npm 依赖纪律

npm 版本依赖是**唯一常态**；源码依赖仅限本地调试，且只能经专门命令进出：

- **默认（提交态）**：所有包的 `@deepseek-ai/*` 依赖钉 registry 版本。上游未修改包直接用官方 `@deepseek-ai/*`（公共 npm 已发布到 `0.1.0-rc.8`，含 `lib/types`；本仓基线随 `runtime/revision.json`）；fork 修改面包用其自有 scope 的发布版（版本形如 `0.1.0-rc.8.zw.3`，见 fork 仓 FORK.md「发布纪律」）。
- **调试（本地态）**：`pnpm run link:source [pkg ...]` 把受管插件的 `@deepseek-ai/*` devDeps 重写为 `link:../deepseek-harness/<subpath>`（锚由 `plugin:setup` 建）并重装；`pnpm run unlink:source` 恢复 registry 版本。映射表（registry 版本 ↔ 源码子路径）在 `scripts/source-deps.mjs` 单点维护，新依赖进映射表才算受管。
- **禁止**：手写 link:/file:/`../` 依赖并提交；以源码 posture 发版；绕过映射表私接源码。发布与 CI 检查在 registry posture 下进行。
- 遗留迁移：`dsh-desktop-bridge`（自有 `dsh` 锚）与 `dsh-mcp-settings`（tsconfig references）尚在 link 形态，按本纪律迁入 `source-deps.mjs` 受管后删除各自 setup 锚——迁移完成前不得新增同类形态。

## 插件契约（dsh-desktop-bridge）

插件是标准 DSH 双面包：`package.json` 带 `dsh.client`（browser half 发现）与 `dsh.bundle`（`dsh plugin add` 激活层）manifest；node half `src/index.ts` 空 apply，唯一作用是让 Loader 行合法（浏览器半经 `exports["./client"]` 发现，参照 `@deepseek-ai/dsh-client-ui-directory-picker-native` 的形态）。

### 环境探测与门控

- 门控信号是 `window.__DSH_DESKTOP__`（壳在 webview 初始化脚本注入）：`{ version: 1, shell: string, platform: string }`。`version` 不认识的整数 → 按 1 处理并 `logger.warn`。
- IPC 走 `window.__TAURI_INTERNALS__.invoke(cmd, args)`（Tauri 2 恒注入）。`__DSH_DESKTOP__` 存在而 `__TAURI_INTERNALS__` 缺失 = 壳契约违约，apply 直接 throw（fail loud，client fiber 失败由 boot 审计上报，不殃及其他插件）。
- 两者皆缺（普通浏览器、终端 `dsh web`）→ apply 立即返回，零注册零副作用：插件恒可挂载、恒无害。

### webview → shell IPC 命令表

壳（Rust 侧）必须注册下列 custom command；插件是唯一调用方：

| 命令 | 入参 | 语义 |
|---|---|---|
| `dsh_desktop_open_external` | `{ url: string }` | 系统浏览器打开 http(s)/mailto 链接。invoke 被拒时插件回退 `window.open(url, '_blank', 'noopener')` 并 `logger.warn`。 |
| `dsh_desktop_notify` | `{ title: string, body: string }` | 原生系统通知（回合完成 / 等待输入）。fire-and-forget，拒绝只记日志。 |
| `dsh_desktop_save_file` | `{ name: string, base64: string }` | 下载桥：把 base64 字节写入用户下载目录（文件名去路径成分，重名自动加 `-N` 后缀），返回落盘绝对路径。M2 起存在。 |
| `dsh_desktop_check_update` | — | 查询更新端点（M3 起）：有更新返回 `{ update: { version, notes } }`，无则 `{ update: null }`；同时推进共享更新状态；未配置/不可达时返回错误文案（软失败，后台指示器静默）。 |
| `dsh_desktop_update_status` | — | 返回进程级更新状态快照：`idle/checking/current/available/preparing/downloading/ready/installing/restarting/failed`；下载态含 `downloaded` 累计字节与可选 `total`，`ready` 表示签名校验完成、正等待用户确认。 |
| `dsh_desktop_download_update` | — | 单飞执行重新检查→逐 chunk 下载→签名校验；校验后的包仅暂存在当前壳进程内，完成后推进到 `ready`，不安装、不重启。 |
| `dsh_desktop_install_update` | — | 只接受 `ready` 状态；消费已校验包并安装，然后自动重启。成功即进程替换，调用方不再收到返回。 |

加命令 = 先改本表，再改两侧。

### 日志汇行（dsh-desktop-log-sink）

桥包随 bundle 层挂载第二个行 `dsh-desktop-log-sink`（`exports["./log-sink"]`，host-only），解决 harness web 组合里 `ctx.logger` 无出口的问题：内建 sink 只有 1000 条内存环形缓冲，console exporter 未挂载，logger 流量不进 stdout，壳的 `desktop-*.log` 与终端 `web-*.log` 都抓不到它。

- apply 注册一个 `ctx.logger` Exporter：每条消息一行 JSON（`{sn, ts, name, type, text[, backfill]}`；`text` 经 `Logger.format` 展开 printf 占位与 Error 栈，对象用 `util.inspect` 防循环引用），追加写入 `logger-<yyyymmdd-HHMMSS>.log`。级别 default=DEBUG（全量）。目录解析与 `web:log`/壳完全一致（`DSH_WEB_LOG_DIR` → `$DSH_HOME/logs`），`logger-latest.log` 软链指向最新（unix-only）。
- 挂载时先从环形缓冲 backfill 启动早期消息（记录标 `backfill: true`）；进程级状态（文件路径 + sn 水位）放 `globalThis`，HMR 重挂载按水位去重、同一文件续写不新建。
- 写盘失败为尽力而为：报一次 stderr（被壳 tee 捕获）后自闭，绝不把异常抛回日志调用方。
- 该行随桥 bundle 层生效，终端 `dsh web`（同一 web profile）也会启用——刻意如此：终端同样没有 logger 出口。文件与 `web-*`/`desktop-*` 同家族，不轮转，手动清理。

### 壳实现要点（M1，`src-tauri/`）

- **sidecar 启动**：按 `find_runtime` 解析出的 runtime 启动 sidecar——`node [<--import tsx/esm>] <cli> web --port <N> --no-open`（`--no-open` 自 harness rc.8 起存在：rc.8 的 `dsh web` 就绪后默认把 URL 交接给系统默认浏览器，桌面壳自有窗口必须显式退出；rc.7 解析器 `allowUnknownOption`，旧 runtime 对该 flag 无害且本就不开浏览器），`<cli>` 为 runtime 树的 `dsh/lib/bin.js`（release 解压树与 `runtime/build/<sha>` 同款，无需 tsx），仅源码兜底才是 `apps/cli/src/bin.ts`（tsx 预载）。不经 pnpm——pnpm 会插一层孙进程导致 SIGKILL 孤儿 node；直接 node 子进程可干净回收。**PATH 分层**：profile `plugin add/install` 只在 runtime tools 之后继承进程 PATH；仅长驻 `dsh web` sidecar 再于 runtime tools 之后、继承 PATH 之前补入实际存在的 Homebrew、`~/.local/bin`、pnpm/npm/cargo 等常见 host CLI 目录，让 Finder/Dock 启动也能解析插件调用的宿主 CLI，同时 release runtime 的自带 pnpm 始终排在用户 shim 之前；源码 dev fallback 仍按开发者继承 PATH 解析 pnpm。**runtime 解析顺序见「运行时分发决策·壳的 sidecar 解析顺序」**（`$DSH_DESKTOP_RUNTIME` → `runtime/revision.json` 钉的 `runtime/build/<sha>`（repo 存在时优先＝dev 主路径）→ 资源解压树（仅 release；`release_runtime_dir` 带 `debug_assertions` 守卫，dev 构建永不消费——`~/.dsh-desktop` 解压树可能属于另一安装的旧 revision，2026-08-20 黑屏第二案根因，详见 `docs/notes/2026-08-20-rc45-runtime-resolution-and-plugin-contracts.md`）→ 源码 checkout 兜底）；源码兜底的 checkout 发现：`$DSH_CHECKOUT` → 本仓同级 `../deepseek-harness` → `~/workspace/deepseek-harness` 惯例位（校验 `docs/architecture.md` + `apps/cli/src/bin.ts`），与 `scripts/setup-plugins.mjs`/各包 `scripts/setup.mjs` 的候选序一致。
- **DSH_HOME 所有权**：默认共享真实用户 home 下的 `.dsh`（Unix `$HOME/.dsh`，Windows `%USERPROFILE%\.dsh`）——桌面与终端是同一账号的两个面（会话历史、工作区、settings、credentials 全部同源可见）。`$DSH_HOME` env 可强制隔离。`~/.dsh-desktop/` 只放壳私有编排数据：`logs/install.log`、`profile-adoptions/<home-hash>/` 经短期跨进程 append lock 串行化的追加式接管状态（损坏记录隔离保留、同 revision 歧义降为 `ConsentRequired`，不得永久阻断启动）、`profile-backups/<home-hash>/<backup-id>/` 可恢复 Web Profile 快照；**sidecar 的 harness 输出走 fork 的 `web:log` 约定**：每次启动一个 `$DSH_HOME/logs/desktop-<yyyymmdd-HHMMSS>.log` + `desktop-latest.log` 软链（与终端 `web-*` 同目录、前缀区分，`DSH_WEB_LOG_DIR` 可覆盖目录；软链 unix-only，Windows 尽力而为、失败则只有 per-boot 文件）。⚠️ 并发注意：harness 对同一 DSH_HOME 没有多进程锁；单用户下基本安全（会话是 per-session JSONL，JSON storages 是整文件 last-wins 原子写），但同一会话同时被两个面驱动是未定义行为；协调式单实例是壳 M2 项。**首次接管现有共享 Home 的授权边界**：恢复完壳自己遗留的 Profile 事务后、任何新 mutation 前检查接管状态；既有 Home 必须原生提示 `备份并继续 / 查看变更 / 退出`，说明只更新共享 Web Profile，并保留 sessions、credentials、settings、`.agent-presets`、home patch、其他 profiles 与其他插件；空 Home 不提示但先落 `Adopting` 记录并以 `ProfileExpectation::Missing` 双重检查，防下一次把 Desktop 自建数据误认作用户既有数据，也防检查后终端新建 Profile 的 TOCTOU；发现该竞态则转 `ConsentRequired`、退出并在下一次重新提示。确认后把当前 `profiles/web` 配置（排除可重建 `node_modules`）持久备份到壳私有目录，复制前后校验源身份，安装事务再以该身份做 CAS；已运行过旧版 Desktop 的用户只能诚实备份“当前 Web Profile”，不宣称是历史 pre-Desktop 状态。每次启动把 bridge 与 compaction 作为**同一个 Profile 事务**幂等安装（两者目标 realpath 均一致时跳过）：壳在真实 `DSH_HOME` 同级创建等深、同卷的 shadow home，复制 web Profile 的全部配置但不复制 `node_modules`，并把 home 根 `cordis.patch.yml` 只读复制到 shadow 供完整配置校验；在 shadow 中执行旧闭包 frozen install → 两包 `plugin add` → 新闭包 frozen install → 原依赖 realpath/受保护配置/`--dump-config` 校验；全部成功后才以 immutable journal + append-only phase records + rename 提交整棵 `profiles/web`。失败时真实 Profile 不动；提交中断由下一次启动按 durable phase 与 profile marker 确定回滚或收尾；另一个桌面进程持有 journal 或提交前及 `real -> backup` 后检测到真实 Profile 配置、home patch 或 `node_modules` 顶层身份被终端改动时回滚并 fail loud；backup 删除前再复核一次。安装失败必须在 sidecar 启动前原生展示 `重试 / 恢复已保存备份 / 退出`（无备份则没有恢复项），所有 native dialog backend 失败都按退出处理，consent/破坏性动作的默认焦点也是退出；`DSH_DESKTOP_DIALOG_DEFAULT` 自动选择仅在 debug build 或 `DSH_DESKTOP_E2E_PROBE=1` 生效；恢复先追加 `RestorePending` 并绑定当前 Profile 身份，再把快照复制到 shadow、frozen install、完整校验后走同一 journal 提交，因崩溃重启时只收尾已匹配的快照或按 CAS 重试，绝不覆盖恢复请求之后的终端修改（当前 Profile 已删除时允许以 `Missing` CAS 重建）；若终端已产生新内容或恢复持续失败，用户可显式“保留当前 Profile”转 `RestoreAbandoned`，下次重新授权，不能形成永久重试死循环。备份损坏/缺失必须原生 fail loud，可保留当前 Profile 并转 `ConsentRequired`，但不得静默使用或删除该备份；后续清理也保留校验不通过的旧目录。成功转 `Restored`、提示后退出，下次启动重新征求授权。所有其他致命启动错误也必须原生可见，不能只写 stderr。bridge 的 bundle 层进入 web Profile；compaction 的空 bundle 层只登记可解析包。Windows 原生进程读 `%USERPROFILE%`，不用 Git Bash 的 `$HOME=/c/Users/...`（那不是 Win32 路径）。
- **端口**：`TcpListener::bind("127.0.0.1:0")` 取随机口，就绪探测 `GET /`（webserver 的 SPA index 路由）状态 2xx（500ms 间隔，120s 超时；tsx 冷启动慢）。
- **WKWebView 已知坑（已修）**：webserver 对 loopback 并发 **chunked** 响应（无 content-length）会被 WKWebView 随机挂死/加载失败（39 个 boot bundle 突发时必现；Chrome 无此问题）。修复在 harness `packages/client/modules/src/index.ts` 的 serveBundle 显式 `content-length`；sidecar 从源码运行改源即生效，值得上游到 fork。
- **窗口**：就绪后主线程建 `main` 窗口（1400×900）加载 `http://127.0.0.1:<port>`；初始化脚本注入冻结的 `window.__DSH_DESKTOP__`（platform = `std::env::consts::os`）。macOS 用 `TitleBarStyle::Overlay`：红绿灯悬浮进页面、不画原生标题栏。注意 Overlay 只设 `fullSizeContentView` + `titlebarAppearsTransparent`，**窗口标题文本仍会画进悬浮带**（与页面 logo 重复的「DeepSeek Harness」就是这么来的）——建窗后经 objc2-app-kit 调 `NSWindowTitleVisibility::Hidden`（`hide_painted_title`；直接依赖 `objc2-app-kit`，本就经 tao/wry 在依赖树内）不画标题但保留字符串给 Mission Control / Window 菜单；**红绿灯按 Electron `WindowButtonsProxy` 钉位**（`inset_traffic_lights`：改 close.superview.superview 即 titleBarContainer 的 frame，把它钉在窗口顶边，再在容器内放三钮——只改按钮会输给缩放动画中的系统 layout。目标：圈中线 y19 对齐带内开关 `top:8/22`，红灯左缘 x16 对齐侧栏字标线。`observe_titlebar_layout` 订 `NSWindowDidResize`（缩放动画每一帧都发；Tauri 的 `Resized` 往往只在动画结束才发，太晚）+ `DidEndLiveResize` + 进出全屏；`WillEnter/WillExitFullScreen` 先藏容器防跳，结束后 redraw 复位）；桥插件侧配合见「功能面」标题栏融合条目。其他平台保留原生标题栏。
- **IPC 能力**：capability 授 `core:default` + `core:window:allow-start-dragging` + `core:window:allow-internal-toggle-maximize`（拖拽条用，Tauri 2 `data-tauri-drag-region` 的运行时命令）+ 全部 `allow-dsh-desktop-*` 权限（`build.rs` 的 `AppManifest::commands` 自动生成，标识符把下划线转连字符），remote urls 模式 `http://127.0.0.1:*`（随机端口）。
- **命令后端**：open_external 按平台 `open` / `cmd /C start "" <url>` / `xdg-open`，先做 scheme 白名单（http/https/mailto/tel）；notify 用 `osascript display notification`（darwin）/ PowerShell WinRT toast（windows，AppId `dev.dsh.desktop`）/ `notify-send`（linux），title/body 做引号或 XML 转义。
- **sidecar 监护（已落地，含优雅退出阶梯与进程组 / Job Object）**：Unix 上 sidecar spawn 进**独立进程组**（`process_group(0)`），终止信号打 `-pgid`——一次内核调用原子覆盖 sidecar 全树（harness 自己 spawn 的 MCP server、工具子进程），无树遍历、无 TOCTOU；`setsid` 逃逸者由下次启动清扫兜底。Windows 上 sidecar 进带 `KILL_ON_JOB_CLOSE` 的 Job Object（壳崩溃即杀整树）；Assign 失败（父进程已在禁止 breakaway 的 job 里）则降级 `taskkill /T`。壳捕获 SIGINT/TERM/HUP（handler 只写原子量，poller 线程执行关闭；Windows 走 `RunEvent::Exit`），退出统一走 **组 SIGTERM→3s→组 SIGKILL 阶梯**（Windows：`taskkill` WM_CLOSE → 3s → `/F`），`RunEvent::Exit` 同路径，收尾后删除自己的注册条目。防孤儿第二道保险——**stale-sidecar 注册表** `~/.dsh-desktop/sidecars.json`：spawn 时记录 `{sidecar/shell pid + 启动时刻, port, log}`，每次启动先清扫再 spawn。清扫**只作用于注册表内 pid**（绝不按进程名扫表，终端 `dsh web` 不可能被误伤）；pid 复用由启动时刻等值比较挡住（Unix `ps lstart`，Windows `GetProcessTimes` FILETIME；复用 pid 的时刻与记录不符 → 视为死）；注册表损坏 fail-open 读空。清扫决策：shell 活 & sidecar 活 → 保留（另一在跑的壳所有）；sidecar 死 → 忘记；shell 死 & sidecar 活 → 孤儿，走阶梯回收后忘记。上游跟踪：tauri#14443（sidecar 树杀 + PID 注册表 PR）与 plugins-workspace#1332（shell 插件 process-group 选项，process-wrap）均已留评论分享实测数据。
- **dev-loop 坑的最终结论（tauri-cli 2.11.4 源码核实）**：`tauri dev` watcher 重建用 `Child::kill()` 杀壳——Unix 上即 **SIGKILL、不可捕获**，壳的退出路径不可能跑到；tauri-cli 唯一的树杀（kill-children.sh）只覆盖 beforeDevCommand 子进程，从不清理 app 自己的后代；其内置忽略表（`node_modules/ target/ gen/ Cargo.lock .DS_Store`）**不含 `icons/`**，改图标同样触发重建。⇒ watcher 重建必留孤儿 sidecar（reparent 到 launchd，持端口与 `~/.dsh`）；上述注册表使**下一次启动自动回收**（已实测：重建 → 新壳日志 `reaped stale sidecar pid=…`）。SIGKILL 场景只能事后回收，这是设计边界而非缺陷；协调式单实例（M2）在其上再做端口/会话协调。
- **e2e 探针**：`DSH_DESKTOP_E2E_PROBE=1` 时壳在页面加载后经 `window.eval`（主线程调度，wry 约束）注入探针 JS（gate→app-root→badge DOM→save_file IPC 往返），verdict 经 IPC 命令 `dsh_desktop_e2e_report`（`dsh-e2e-` hash 兜底）；壳轮询 IPC 结论并打日志。配 `DSH_DESKTOP_E2E_EXIT=1` 自动退出：0 通过 / 2 失败 / 3 超时。注意：`window.title()` 与 `document.title` 在 macOS 上不同步，标题不能做 verdict 通道。

### 功能面

M1（已实现）：

1. **外链路由** —— document 捕获阶段 click 监听：`target=_blank` 的锚点、跨源 http(s) 锚点、`mailto:`/`tel:` → `preventDefault` + `dsh_desktop_open_external`。同源无 target 的锚点、`#`、`javascript:`、`blob:`/`data:` 一律放行（SPA 内部导航）。判定是纯函数 `classifyAnchor`（`src/client/links.ts`），单测覆盖。
2. **注意力通知** —— 订阅 `ctx.sessions.list`（raf 批量快照流），做状态转移 diff（纯函数 `diffAttention`，`src/client/attention.ts`）：`running: true→false` 或 `pendingInteraction: 无→有`，且通知时刻 `document.hidden`，发 `dsh_desktop_notify`；一轮转移同时出现两种边时只发「等待输入」一条。标题用 `displayTitle`。后台会话（未选中）同样通知——这是桌面形态的核心价值。
3. **web 端指示** —— `shell.overlay`（加性 list 槽，全帧浮层）注册 `desktop-badge` 条目：右下角小 pill「web端」，点击以 `dsh_desktop_open_external` 打开当前 origin（复制会话到系统浏览器）。样式只用 `--dsw-*` 语义 token，绝不写字面色。
4. **标题带更新入口** —— 仅经 `shell.overlay` 插件实现：挂载 3s 后首查，之后每 2h 强制刷新；离线、无端点或已是最新版时完全静默。macOS 发现新版后在左上角标题带的侧栏开关旁出现更新控件（收起态的 `+` 新会话气泡仍在其右侧）；其他平台保留右上角 fallback。**发现新版即后台自动下载**（同版本每会话只自动一次；失败保留可点重试），按钮原位旋转并显示底部进度条（有 `total` 时定长百分比，否则不定长动画），`dsh_desktop_update_status` 提供实时字节进度；签名校验完成进入 `ready` 并弹确认框，只有确认“安装并重启”才消费暂存包、安装和重启。检查、下载、安装各自单飞，自动下载不等于授权安装。

M2（下载桥与 i18n 已实现；其余规划，先改本表再动手）：

- ~~下载桥~~（已实现）：捕获 `a[download]` 点击（同源 http(s) 与 `blob:`，纯函数 `classifyDownload` 判定）→ fetch blob → base64 → `dsh_desktop_save_file`；invoke 失败回退 `location.href` 导航下载。
- ~~badge 文案接 `ctx.locale` 双语~~（已实现，namespace `desktop-bridge`）。
- ~~标题栏融合（macOS）~~（已实现）：壳建窗用 `TitleBarStyle::Overlay` + `NSWindowTitleVisibility::Hidden`（不画标题文本，见「壳实现要点·窗口」）；桥插件在 `platform === 'macos'` 时（纯函数 `shouldFuseTitlebar`，`src/client/titlebar.ts`）注入一条 CSS——`div:has(> [data-shell-overlay])>div:nth-child(-n+3)` 各加 28px `padding-top`（选择器锚点是 ui-layout AppFrame 的三列：sidebar/center/details；给列而非 frame 加 padding，让列的表面（侧栏填充）**铺到红绿灯底下**、只有内容避开悬浮带，而不是整帧下移留出空白条）——并注册第二个 `shell.overlay` 条目 `desktop-drag-strip`（`data-tauri-drag-region` 透明拖拽条，单击拖动、双击切换最大化，走 capability 的两个 window 权限，不加自定义 IPC）。视觉结果：侧栏 surface 从窗口顶边铺开，红绿灯直接压在侧栏色块上，侧栏内容（logo 行）从带下方开始，与原生 mac 应用的融合标题栏一致。已知边界：拖拽带盖住侧栏 resize handle 顶部 28px（z-index 20 > handle 2）；Overlay 窗口未聚焦时不可拖（Tauri #4316）；`nth-child` 锚点假设 AppFrame 的三列仍是 frame 的前三个子元素（ui-layout 结构变更需同步此选择器）。
- ~~收起侧栏整列隐藏 + 标题带控制钮（macOS）~~（已实现）：Overlay 标题栏下，ui-layout 的「收起」仍是 56px 控制轨（`SIDEBAR_COLLAPSED`，rail 里有 logo/新建会话/设置图标）——这条 rail 垫在红绿灯正下方成为无交互死条。桥插件在 `platform === 'macos'` 时（与标题栏融合同一门控）把收起列压到 0 宽，并隐藏侧栏 logo 行里的原生 toggle（**BrandWordmark 保留显示**，锚点 `div[data-slot='sidebar']>div>div:first-child>button:last-child`——`data-slot` 是 slot 系统文档化的稳定锚点，Tooltip 无包裹 DOM，logoRow 的最后一个按钮即原生 toggle）：桌面全窗口**只保留一个侧栏开关**——红绿灯右侧、28px 标题带内的常驻双向 toggle，收起时其旁滑入仅收起态可见的新会话气泡（`src/client/rail.ts` + `rail-controls.tsx`）。机制：列宽在 frame 的 inline `grid-template-columns`（`<sidebar>px minmax(0,1fr) <details>px`），纯 CSS `!important` 覆盖整条模板会丢 details 动态宽度，故用 MutationObserver 在 `data-sidebar-collapsed` 期间把第一轨改写为 `0px`（纯函数 `collapseRailTemplate`，只认「`<num>px` 开头且后随轨道」的模板形状，失配原样放行、功能退化为原生 rail；React 重渲染重写 style 后 observer 同 microtask 再纠正，无闪烁；frame 自带 grid 轨道 transition，收起 56→0 / 展开 0→280 均为平滑动画；React 不回读 DOM style 做 diff，外部改写稳定）；按钮是第三个 `shell.overlay` 条目 `desktop-rail-controls`（order 5，`top:8px;left:86px;height:22px;gap:8px` 红绿灯右侧、与下移后的灯排同线（中线均为 y≈19；与绿灯圈右缘留约 12px），z-index 1 压过拖拽条——占约 26px 带内区域不再可拖窗，与原生工具栏按钮同理）：toggle **常驻**、双向（收起/展开同钮同图标，无入场动画）；发现更新后紧接更新控件并后台自动下载，下载阶段原位旋转+底部进度条、校验完成弹确认框；新会话气泡**仅收起态**，用 `opacity/transform/visibility` 过渡（`display` 无法动画）在其旁从 `translateX(12px)` 滑入（delay .18s 接在侧栏滑动后），展开时反向淡出，`prefers-reduced-motion` 去过渡；容器恒 `pointer-events:none`，toggle 恒可点、气泡仅可见时可点：toggle 调 `ctx.layout.toggleSidebar()`——**点击时惰性 `ctx.get('layout')`**，绝不在注册时读取：slots.inject 在 ui-layout 声明落地（其 fiber 启动途中、尚未 ACTIVE）即触发，而 strict `ctx.get` 只服务 ACTIVE 提供方，注册时读取会拿到 undefined 导致按钮永不出现（2026-08-19 实踩，缺席仅 warn 并忽略点击）；新会话调 `ctx.workspaces.startSession()`（无参 = 侧栏按钮同款语义；inject 加 `workspaces`）；图标用 ui-primitives 的 `IconPanelLeftOutline16` / `IconNewChatOutline16`（与 rail 原图标一致），样式全在 `railCss()`、只用 `--dsw-*` token。已知边界：DOM 锚点依赖 ui-layout 的 `data-sidebar-collapsed` 属性与 inline 三轨模板（ui-layout 结构变更需同步 rail.ts，与 `nth-child` 锚点同性质）；收起态下 rail 的 workspace 浏览/设置入口不可达（新会话由带内按钮补齐，其余需展开后用）。
- 通知点击回跳：壳发 `dsh-desktop://focus-session` 事件，插件聚焦并 `sessions.open(id)`。**受阻**：macOS 通知点击回调需 UNUserNotificationCenter delegate（objc2 绑定），`osascript` 无回调通道——留待 M3 平台化一并做。同理**通知横幅图标也受阻**：osascript 通知恒归属 Script Editor，要图标必须有真实 .app bundle 身份——osacompile applet 捷径已证伪（run 事件投递不可靠、`open` 对未识别 bundle 会 fallback 到 Terminal 开窗，详见 `docs/notes/2026-08-19-notify-applet-incident.md`），勿再尝试；正路同归 M3 的 UNUserNotificationCenter。
- 托盘 / 未读角标（壳读 DOM title 或插件显式上报）。

### 组合与 slot 纪律（沿用 DSH client 约定的最小子集）

- UI 只经 `ctx.slots.register(...)` 组合；本插件只注册已声明的加性槽 `shell.overlay`（badge/拖拽条/带内 rail 控件，更新入口嵌在 rail 控件内），声明洞一律禁止。品牌字标等"始终挂载"关注点不归桥（见 roster 的 `dsh-branding`）。
- 跨包只走 slot 与 ctx 服务，禁止 import 其他插件的实现符号；harness 包只做 type-only import（构建时擦除）。
- 注册即 effect：所有监听、订阅、slot 注册经 `ctx.effect()` / register 返回的 disposer，卸载/HMR 全量回收。
- 文案中文（M2 起接 `ctx.locale` 双语）；代码注释英文。
- 无硬编码 tunable：可调项（如通知开关）是 `Config` 字段，从 cordis.yml `config` 进来，非法值 fail loud。

## 壳（Tauri 2）契约要点

壳对插件只有两个义务：初始化脚本注入 `window.__DSH_DESKTOP__`（见上），注册 IPC 命令表（见上）。其余职责不变：spawn harness sidecar（`dsh web`，随机回环端口）、`GET /` 就绪检测（host.describe 是 RPC 方法名，不是 HTTP 路由）、窗口加载 `http://127.0.0.1:<port>`。生产形态把本插件经 `dsh plugin --profile web add` 装进随包 profile（自带 `dsh.bundle` 层，无需 `--patch`）。

## Commands

前置：Node 22+、pnpm；类型检查与构建另需 DSH 源码 checkout（发现顺序：`$DSH_CHECKOUT` → 本仓同级 `../deepseek-harness` → `~/workspace/deepseek-harness` 惯例位，验证标准 `$DSH/docs/architecture.md` 存在）。

```sh
pnpm run plugin:setup     # 根级：建 plugin/deepseek-harness 锚（link:source 与 mcp-settings 用）+ 桥自己的 dsh 锚
pnpm run plugins:check    # 全树：plugin/* 每包跑自己的 typecheck/test/build（--if-present，跳过 symlink 锚）
pnpm run link:source      # 调试：受管插件 devDeps 切 link: 源码（见「npm 依赖纪律」；不可提交）
pnpm run unlink:source    # 恢复 registry 版本（提交态）

cd plugin/dsh-desktop-bridge
pnpm install          # 安装 devDeps（tsdown/typescript/tsx/react 类型）
pnpm run typecheck    # tsc --noEmit（harness 包 import 经 dsh 链接解析到源码）
pnpm run build        # tsdown：lib/index.js + lib/invariant.js + lib/client.js
pnpm run test         # node --import tsx --test（纯函数单测）
pnpm run watch        # tsdown --watch（配合 dsh web 的 client-hmr 热替换）
```

mcp-settings 在包内自带 pnpm 11（packageManager）与 vitest 工具链，`cd plugin/dsh-mcp-settings && pnpm install && pnpm test` 独立可用；provider-balance 无构建步骤（裸源码分发，收敛进 tsdown 纯度门是后续项）。

### 实机挂载验证（scratch home，勿污染真实 `~/.dsh`）

```sh
export DSH_HOME=$(mktemp -d)
cd $DSH_CHECKOUT
pnpm dsh plugin --profile web add <repo>/plugin/dsh-desktop-bridge
pnpm dsh web --port 3987 &
curl -s localhost:3987/ | grep -o 'dsh-desktop-bridge[^\"]*'   # boot graph 应含本插件行
curl -sI localhost:3987/plugins/dsh-desktop-bridge/client.js   # 应 200
```

### 壳的运行与端到端验证（M1 起）

```sh
# 前置：Rust toolchain（rustup）、Node 22+ 与 DSH checkout（发现顺序见「壳实现要点」）
pnpm desktop:dev                # dev 壳：spawn sidecar → 就绪 → 开窗
# e2e（探针走 gate→badge DOM→save_file IPC 往返，结论打在 stdout；EXIT 变体自动退出）
DSH_DESKTOP_E2E_PROBE=1 pnpm desktop:dev
DSH_DESKTOP_E2E_PROBE=1 DSH_DESKTOP_E2E_EXIT=1 pnpm desktop:dev; echo "exit=$?"
```

壳的 sidecar 默认跑在真实 `~/.dsh`（与终端同源）；harness 输出落 `~/.dsh/logs/desktop-<时间戳>.log`（`desktop-latest.log` 软链指最新，`DSH_WEB_LOG_DIR` 可覆盖），`~/.dsh-desktop/logs/` 只落 `install.log`。浏览器内验证桌面行为以 `window.__DSH_DESKTOP__` 手工注入为辅助手段。

## Conventions

- ESM（`"type": "module"`）；插件包名无 scope，目录名 === `package.json` `name`，随仓分发（见「插件 monorepo 规范」）。
- client bundle 构建契约（banner/footer/externals）从 DSH `packages/client/tsdown.client.ts` 蒸馏：产物是 `window.__ModuleLoader__.load({id, factory})` 闭包；externals = 平台模块表**rc.8 起的隐式基线**（react/cordis/ui-slots/ui-primitives + runtime 豁免——rc.8 把 `web-react`/`ui-attachment`/`schema-form` 移出 PLATFORM_MODULES 改为普通内联库，并新增按包 `dsh.client.external` 声明机制；桥的镜像表见 `plugin/dsh-desktop-bridge/tsdown.config.ts` 注释）；非基线 `@deepseek-ai/*` 值 import 一律构建报错（纯度门）。基线 bump 时该镜像表必须跟着 `PLATFORM_MODULES` 核对。
- 纯函数与副作用安装分离：判定/diff 逻辑无 DOM 依赖可单测；安装函数薄壳包 effect。
- 空不发声、缺即报错：可选服务 `ctx.get()` 处理 undefined；配置缺引用在能定位的最早点 throw。
- 组件不做订阅机械（useSyncExternalStore 等）；快照流消费在 apply 世界订阅、经闭包注入。
- 文件恰好一个行尾换行；`git diff --check` 干净。
- 非平凡变更加 Agent Note（`docs/notes/`，日期命名）记录决策与理由。

## Milestones

仓库整体（README 详述）：M1 Tauri 原型（脚手架 + sidecar + 端口 + 就绪 + 窗口）→ M2 对齐 dataelement 行为 → M3 平台化（签名/更新/安装包）→ M4 系统 WebView 回归。

### 运行时分发决策（已定，M3 实现）

**runtime 整树不发 npm**（它是自包含安装产物：CLI 树 + node/pnpm 二进制）。fork 的 GitHub 仓库仍是源码事实源，但**对 fork 修改面的消费走 npm**（fork FORK.md「发布纪律」：修改包以自有 scope 发 `<上游版本>.zw.<N>`）：`prepare-runtime.mjs` 对 FORK_MODIFIED 集合的 overrides 指向 `npm:@crazx/<pkg>@<版本>.zw.<N>`（npm 上不存在时 fail loud，先在 fork 仓发 zw 版再组装），其余包仍从 fork clone 打 tarball 钉本地。发包以 **`v<基线>+zw.<补丁>` 标签**为锚（semver build metadata 标识 zw fork；历史 `desktop/vX.Y.Z` 标签等价有效）：`runtime/revision.json` 钉 `{repo, ref: v<基线>+zw.<n>, sha}`，fork 侧 `git tag v<基线>+zw.<n> <sha> && git push origin <tag>` 后更新本文件。当前：`v0.1.0-rc.8+zw.4`（harness 基线 0.1.0-rc.8；zw 层 4＝publish-fork 修复 vendor 线 workspace 依赖改写——`@crazx/*` 曾把 schemastery/cordis-plugin-* 等钉到 fork 基线版本，peer 边绕过 overrides 直查 registry 即 404，zw.2/zw.3 均带毒、zw.2 仅因本地 tarball 恰好全覆盖而侥幸出货，zw.4 按目标包真实版本线改写并顶替 zw.3；层 3＝基线升 rc.8 + `dsh-tool-cordis` 补进 FORK_MODIFIED + publish 基线断言改 `--base`；层 2＝frontend-static content-length）。FORK_MODIFIED 消费面以 fork 仓 `node scripts/publish-fork.mjs --list` 为准（基线 bump 后跑一次核对），本文件只记当前快照。

组装（`node scripts/prepare-runtime.mjs`，SHA 键控缓存，同 SHA 秒级）：持久部分克隆 fetch 标签 → `pnpm install --frozen-lockfile` + `pnpm run build`（`.prepare-runtime-ok` 标记缓存）→ **publish 路径打本地 tarball**（`pnpm pack` 全部 234 个 `@deepseek-ai/*` 包，workspace: 协议按发布规则重写；平台特定原生包 landlock-linux 跳过回退 npm；`FORK_MODIFIED` 名单内的包打包失败即中止）→ 生成的 runtime manifest 以 `pnpm.overrides` 把全树钉到本地 tarball（**必须 `--no-frozen-lockfile`，frozen 模式会静默忽略 overrides**；`pnpm deploy --legacy` 对本 workspace 丢 vendored 传递依赖，不可用）→ `runtime/build/<sha>/{dsh,tools}`（dsh = CLI 树，tools = node 24.9.0 + pnpm 二进制）。

壳的 sidecar 解析顺序：`$DSH_DESKTOP_RUNTIME` → **`runtime/revision.json` 钉的 `runtime/build/<sha>`**（repo 存在该树时优先——dev 主路径；`find_runtime` 把它排在资源解压树之前，防 `~/.dsh-desktop` 里属于另一安装的旧 revision 劫持 dev）→ **包内资源解压树**（仅 release：`release_runtime_dir` 带 `cfg!(debug_assertions)` 守卫，dev 构建直接跳过此分支。`~/.dsh-desktop/runtime/<sha>/{dsh,tools}`，首启从 Resources 里的 runtime.tar.gz 原子解压，`.ok` 标记完整；bridge 与层次压缩分别按 manifest 中自己的 tarball hash 解压到 `~/.dsh-desktop/{bridge,plugins/dsh-compaction-hierarchical}`。解压包不带 `node_modules`：壳在 `plugin add` 后把 bridge 的 Cordis 以及 compaction 的六个 Harness peers 链到 runtime 树的同一 physical package，避免缺依赖与模块身份分裂；链接指向旧 runtime revision 或悬空时自愈重指，dev package 的真实依赖目录不动。`.ok` 按各 tarball 的 sha256 内容寻址；runtime tar 缓存键另含 assembly script revision 与签名 posture，因此同 runtime git sha 的受管重组装也会传播。若手工修改缓存树却不更新这些键，必须删 `src-tauri/resources/runtime.tar.gz{,.sha}` 后重跑 prepare）→ 本地 fork 源码（dev 兜底，tsx）。e2e 已对 bundled runtime 与资源解压分支（强制 miss dev 路径）验证 `DSH_E2E_OK`。

**FORK_MODIFIED 的 npm 消费细节**：fork 集合不仅走 `pnpm.overrides` 的 `npm:` 别名，还作为 runtime manifest 的**直接依赖**声明——pnpm 的别名 override 只约束普通依赖边，**hoist 兜底（`.pnpm/node_modules`）与 peer 解析不受别名约束**，上游新版本（如 rc.8 匹配 `^0.1.0-rc.7`）会从这些路径漏进官方无修复副本（2026-08-20 黑屏第三案根因）；直接依赖必然解析别名，hoist/peer 随之绑定 crazx 副本。组装后有 fail-loud 扫描：树里残留任何 fork 包的官方 registry 副本即中止。**overrides 必须写在 pnpm-workspace.yaml，不得写在 package.json `pnpm` 字段（2026-08-20 zw.4 发布案，`docs/notes/2026-08-20-pnpm11-overrides-ignored.md`）**：pnpm 11 删除了 package.json `pnpm.overrides` 支持且**静默忽略**——本地 pnpm 升 11 后的组装里 overrides 整表失效，186 个包全走官方构建而版本号相等、扫描按版本放行完全失明；同版本双模块实例分裂 unique-symbol 注册表（bash 工具 `undefined (reading 'prepare')`、typert 远端路由 404 即其切面）。配套：runtime package.json 钉 `packageManager`（组装不随 shell pnpm 漂移）；全部打包 tarball 进直接依赖（file:/alias overrides 都够不着 peer 边，直接依赖给 peer 解析供 root 级实例）；`autoInstallPeers: false`（堵「未决 peer 按 range 从 registry 自动装」旁路）；allowBuilds 对 buildable tarball 直依赖按 `name@file:` 全限定键显式表态；扫描第三桶——同包 `file+` 实例与 registry 实例并存即中止（**按实例来源分桶，不按版本号**；pack-skip 原生包 registry-only 单例合法）。

**基线锁定（上游线纪律）**：runtime 全树钉在 fork 追踪的**单一上游线**——`prepare-runtime.mjs` 把每个非 fork 的 `@deepseek-ai/*` 包 override 到 **fork 树自己 manifest 声明的版本**（vendored 框架线如 schemastery 3.x、原生包 landlock 0.1.1 各按其真实版本线，不套 dsh rc 节奏），skipped natives 同样钉死不浮。**上游发新版不自动跟进**：混两条上游线（rc.7 骨架 + rc.8 末梢）会让 `unique symbol` 注册表跨模块实例失效（`undefined (reading 'prepare')`、credentials 服务"未挂载"——2026-08-20 rc.5 混装案）且静默发生。上游线的移动只经一条路径：**fork 仓合并 upstream → 跑聚焦测试 → 发新 zw 层 → 本仓 bump revision 重组装**——给 fork 留适配兼容时间，官方包永远"协同上游依赖的版本"更新，绝不 `^range` 自由漂移。组装后的 fail-loud 扫描同时检查两件事：fork 包无官方副本、树内 `@deepseek-ai/*` 无偏离 fork manifest 版本的第二上游线。

**已知残留（插件自带 `@deepseek-ai/*` 模块副本，注册表按模块实例分裂）**：profile 里 link: 到本仓工作区的插件（mcp-settings 等）在装配 runtime 下报 "no credentials service mounted"——插件自带独立 cordis 模块副本（其 node_modules 的 registry cordis ≠ runtime 树 cordis，不同 store 路径），Service 注册表按模块实例隔离。源码 `dsh web` 无此问题恰因两者 cordis 同路径（link 到 checkout）。**web-search-toggle 的 `/api/webSearchToggle/*` 404 是同类第二案（2026-08-20，机理定案与修法见 `docs/notes/2026-08-20-pnpm11-overrides-ignored.md`「遗留」节）——已修（插件侧 0.1.1）**：`@Remote` markers 存 typert-protocol 模块级 WeakMap，插件自带 rc.7 副本与 runtime rc.8 实例互不可见（字符串键服务与 binding 跨实例可见，唯独方法枚举不可见，症状隐蔽；tsx 4.22 源码统一解析故源码上无此问题，tsx 4.23 纯 Node 上溯故命中插件副本）。**插件侧修法＝out-of-tree Remote 插件的通用姿势**：Host 行 apply 里把与浏览器端共享的 `TYPERT_REMOTE.descriptors` 经 `ctx.typert.register()` 注册成 Host strict contribution（见 `plugin/dsh-web-search-toggle/src/typert.host.ts`）——strict 路径不依赖 marker 发现，跨实例问题整个绕开；registry 的 `validateCodec` 是 duck check（只验 `schema.parse`），插件自带/内联 zod 副本合法；apply 必须返回 disposer（register 的内部 effect 绑在 registry 的 ctx 上，调用方负责随 fiber 卸载回收）；host bundle 内联 zod（git 安装无 devDeps）。插件 devDeps 升版**无效**（不同物理路径仍是不同实例）。fork 侧正解（未来 harness PR，非本插件所需）：`bindTypertRemote()` 让 binding 携带「从声明模块读取 markers」的闭包——binding 跨实例可见而 WeakMap 不可见；旧装饰器数据封死在旧模块里无法单边兼容，升级协议后插件同步重发。

### 打包（M3 已落地，手册：`docs/packaging-playbook.md`）

`pnpm desktop:build` 一键出平台安装包：macOS `.app` + `.dmg`（aarch64；**签名+公证版 ~160MB**，ad-hoc 降级 ~113MB）；Windows NSIS `*-setup.exe`（x86_64，currentUser，不需管理员）。**runtime 必须在目标 OS 上组装**（native 模块与 node 二进制布局），缓存键含 `platform-arch`。资源走 **tarball** 而非散目录：runtime 树是 pnpm 安装（Unix 3k+ 符号链接；Windows 用 `node-linker=hoisted` 实目录——bsdtar 会把 junction 展开成拷贝，isolated 布局下 `tsx` 会丢 `esbuild`），tauri-bundler 对目录资源不承诺保链接（解引用拷贝会让 .pnpm store 膨胀 GB 级）；tar 往返在 Unix 上链接感知，且解压到 home 规避 App Translocation 只读卷。Windows 解压：GNU tar 需要 `--force-local`（否则 `C:` 被当成远程主机）；Win11 自带 bsdtar 3.8.4 不认该选项且绝对路径可直接解——prepare 与壳按 `tar --help` 探测后按需加 flag。`beforeBuildCommand` 先跑 `scripts/prepare-desktop-bundle.mjs`（bridge + compaction 构建 → runtime 组装（SHA 键控缓存 + `SCRIPT_REV` 组装版本盐）→ 打 `src-tauri/resources/{runtime.tar.gz,runtime-revision.json,bridge.tar.gz,compaction-hierarchical.tar.gz}`，gitignored、按需再生；revision 副本含三个 tarball 的 **sha256**，壳的 `.ok` 缓存按资源内容寻址——同 revision 的任一插件重建都会重解压对应包）。**bundled runtime 与源码 runtime 行为对齐**：tsx 是 runtime 一等依赖，`bundled_runtime` 同样 `--import tsx/esm`——profile 可挂 `file:` 源码分发插件（.ts 入口），纯 Node 拒绝剥 node_modules 下的类型（0.1.0 实踩：真实 home 全树崩溃，scratch home 测不出，**e2e 矩阵必须含真实 home 场景**）。**裸 `cargo build/check` 会因 build.rs 校验资源缺失而失败**，必须先 prepare。分发：**macOS 签名+公证已落地**——`.app`/`.dmg` 均 `spctl` 应答 `source=Notarized Developer ID`（**公证扫描会钻进 tar.gz**，runtime 树 16 个 Mach-O 打 tar 前逐个 Developer ID 签名，`DSH_CODESIGN_IDENTITY` 门控 + allow-jit entitlements；hardened runtime 已开；DMG 需 `notarytool submit` 单独公证；ASC 个人 API 密钥不能用于 notarytool，用 App 专用密码）。DMG 的 Finder 定制布局依赖卷根 `.DS_Store`；Tauri 在 `CI=true` 下默认走 `--skip-jenkins` 只复制背景、不写布局，故 macOS Release job 必须设 `TAURI_BUNDLER_DMG_IGNORE_CI=true`，并在公证前用 `scripts/verify-dmg-layout.sh` 挂载最终 DMG 做 fail-loud 校验。凭据与完整流程见 playbook §5。Windows NSIS 的 Authenticode 按 playbook §9：GitHub Secrets `WINDOWS_CERTIFICATE` + `WINDOWS_CERTIFICATE_PASSWORD`（pfx base64）有则签、空则跳过（SmartScreen 可能警告）；`tauri.conf.json` 不写死 thumbprint。**自动更新（0.1.2 起）**：`tauri-plugin-updater` + `createUpdaterArtifacts`，端点 `releases/latest/download/latest.json`，更新包 macOS `dsh-desktop.app.tar.gz` / Windows `*-setup.exe`，经 tauri 签名私钥（`tauri-keys/`，gitignored）签名、公钥在 `tauri.conf.json` plugins.updater；`latest.json` 的 `platforms` 含 `darwin-aarch64` 与 `windows-x86_64`，由 macos+windows 两 job 出 fragment、publish job 合并。发布流水线 `.github/workflows/release.yml`（tag 触发），发布手册 `docs/release-runbook.md`。

插件（本文件「功能面」的 M1/M2）；壳的 Rust 侧实现需本机 Rust toolchain（stable，已就绪）。
