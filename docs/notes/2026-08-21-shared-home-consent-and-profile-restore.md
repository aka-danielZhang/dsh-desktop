# 共享 DSH_HOME 的授权、备份与恢复

日期：2026-08-21

## 问题

Desktop 默认与终端 DSH 共用真实用户的 `DSH_HOME`。这是会话、工作区、设置和凭据同源可见的产品语义，但壳也会在启动前向共享 `profiles/web` 安装 desktop-owned 插件。

旧启动链有两个缺口：

1. 首次遇到已有 DSH 数据时直接修改 Web Profile，用户不知道 Desktop 会写什么，也没有明确退出机会。
2. 安装或配置校验失败只落 stderr/install.log；主窗口尚未创建，用户看到的是应用退出或空白，没有可操作的修复路径。

问题是真实的，但修改面不是整个 `DSH_HOME`。PR #8 已把安装收进 shadow Profile + journal 事务，本变更在该事务外增加授权和长期恢复契约，不退回原 PR #4 的原地、非 frozen 修复路径。

## 决策

### 首次接管

壳恢复自己遗留的 transaction journal 后、任何新 Profile mutation 前检查 shell-private adoption record：

- 空或全新的 Home：不提示，先记录 `FreshHome / Adopting`，并以 `ProfileExpectation::Missing` 在 skip 前和 transaction 内双检；若终端随后创建 Profile，则转 `ConsentRequired`、退出并在下次重新提示。安装成功后转 `Active`。
- 有数据但没有有效 adoption record：提示 `备份并继续 / 查看变更 / 退出`。
- `Adopting`、`Active`：继续既有授权，不重复提示。
- `RestorePending`：先完成或收尾恢复，绝不静默重装 desktop packages。
- `ConsentRequired`、`Restored`、`RestoreAbandoned`：视为当前授权不可继续，下一次重新提示并创建新备份。

提示明确写出只更新共享 Web Profile，并保留 sessions、credentials、settings、`.agent-presets`、home root patch、其他 profiles 和其他插件。Windows 的标准 MessageBox 不能自定义按钮文字，因此正文显示 Yes/No/Cancel 到业务动作的映射。

所有 dialog backend 失败都映射为 Exit，且 destructive/consent 动作不做键盘默认，不能从失败或回车推导用户同意。`DSH_DESKTOP_DIALOG_DEFAULT=primary|secondary|escape` 只在 debug build 或显式 `DSH_DESKTOP_E2E_PROBE=1` 下用于 CI 和可重复人工探针；production 环境变量不能绕过授权。

### 持久备份

用户确认后，壳把当前 `profiles/web` 的配置、manifest、lockfile、workspace 文件和符号链接复制到：

```text
~/.dsh-desktop/profile-backups/<canonical-home-hash>/<backup-id>/
```

`node_modules` 和 transaction marker 不进入快照；前者可用 lockfile frozen rebuild，后者属于瞬时提交协议。备份带 manifest 和 `.ok` checksum，文件和目录逐层 sync 后才 rename 发布。Windows 会 flush 可写文件句柄，但 Rust 标准库没有目录 fsync，目录 rename 的掉电顺序依赖 NTFS metadata journal；因此启动时仍以 manifest / `.ok` / snapshot identity 重新校验，不能只相信目录存在。

备份前后都计算真实 Profile 身份。安装事务接受 `ProfileExpectation::Identity`，并在 no-op skip 前及 journal 创建内两次检查；若终端在备份后改过 manifest、lockfile、patch、workspace 或顶层依赖身份，事务拒绝执行。此时用户必须显式选择“保存当前状态并继续”以生成新备份，或恢复旧备份；崩溃发生在 Profile commit 与 `Active` 状态记录之间时走同一可判定收尾。Desktop 旧版本已经写过该 Profile 时，只能称为“当前 Web Profile 备份”，不能虚构历史 pre-Desktop 快照。

已发布备份每次使用前校验。校验损坏或缺失时必须原生 fail loud；用户可保留当前 Profile、清除活动恢复点并转 `ConsentRequired`，但磁盘上的不可验证目录不会被静默删除，之后 pruning 也只删除 manifest、completion checksum 与 snapshot contents 全部有效的旧备份。

adoption 状态是壳私有的追加式记录：

```text
~/.dsh-desktop/profile-adoptions/<canonical-home-hash>/
```

追加前用每个 Home 的短期 `create_new` lock 串行化 revision CAS；进程崩溃遗留的 lock 过期后可回收。若旧版本竞态已产生同 revision 多记录，或某条 JSON 损坏/来自未知 schema，文件内容以 `.invalid-*` 隔离保留，但读取结果保守降为 `ConsentRequired`，由下一次明确授权写入更高 revision 收敛，不能形成永久 boot dead-end。

### 失败与恢复

安装失败发生在 sidecar 启动前，原生对话框提供：

- `重试`
- `恢复已保存备份`（只有经过校验的备份存在时）
- `退出`

真实 Profile 仍由 PR #8 的事务保证为完整旧树或完整新树，不暴露半安装状态。

恢复不是直接复制到真实 Home：

1. 记录 `RestorePending`，同时保存恢复请求时的完整 Profile 身份。
2. 校验 backup manifest、checksum、路径归属和快照身份。
3. 若当前 Profile 已与快照匹配，说明上次提交已完成，只补记状态。
4. 否则以恢复请求时的身份做 CAS，在 shadow 中替换为快照并 frozen install。
5. 校验快照配置未被 install 改写、完整配置可 dump，再经同一 journal promote。
6. 转为 `Restored`，提示成功并退出。

这使崩溃后的行为可判定：旧 transaction 回滚后可安全重试，已 promote 则只收尾；若终端在恢复请求后又改过 Profile，CAS 拒绝覆盖该新修改，并允许用户显式保留当前 Profile、转 `RestoreAbandoned`。任何其他持续恢复错误也提供同一退出路径，不能形成只能重试的死循环。若终端删除了 Profile，则以 `Missing` CAS 恢复（不覆盖任何新内容）；检查后若又创建则仍拒绝。

## 非目标与边界

- 不备份或恢复整个 `DSH_HOME`。
- 不复制 sessions、credentials、settings、Agent presets、其他 profiles 或 home root patch。
- 不提供第二套隔离 `DSH_HOME`；产品语义仍是 Desktop 与终端共享账号数据。
- 不为缺少 lockfile 的快照执行非 frozen 恢复；这种状态 fail loud，真实 Profile 保持不变。
- shell-private 备份是单机恢复点，不替代用户自己的长期备份策略。
