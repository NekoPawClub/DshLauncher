# AGENTS.md — DshLauncher 项目指南

DeepSeek Harness (dsh) 的 Windows 系统托盘守护启动器，Rust 编写。
本文件汇总项目架构、编译经验与工具调用注意事项，避免重复踩坑。

## 项目概述

- 作用：常驻系统托盘，作为 dsh 的守护程序 (watchdog)，保证 dsh 持续运行
- 启动命令：`npx -y @deepseek-ai/dsh web` (后台隐藏窗口，端口覆盖时附加 --port)
- 技术栈：Rust 1.97 / tray-icon 0.19 / winit 0.30 / image 0.25 (仅 ico 特性) / windows 0.61 (WinRT toast) / windows-sys 0.59
- 目录结构：
  - `src/main.rs`：托盘、菜单、watchdog、启动流程、动画；启动失败与 panic 写入 launcher.log (windows_subsystem 无控制台)；watchdog/动画循环逐次 catch_unwind，异常后继续服务
  - `src/dsh.rs`：dsh 进程控制 (启动/停止/端口探测)、ShellExecuteW、纯 Win32 进程树清理、单实例；兜底清理用完整镜像路径 + 命令行确认 dsh 身份
  - `src/log.rs`：单文件日志 `launcher.log` (启动器事件 + dsh 输出 `[ DSH]`，按行时间标签 + 凌晨 4 点日界，保留 3 天；超过 10 MiB 按行截断到 8 MiB 内；写入/查询/清理全部由后台日志管理线程串行处理，其它线程只投递消息队列；单条消息处理异常后线程继续)
  - `src/toast.rs`：直接通过 WinRT 发送更新 toast，并登记自有 AUMID (不再启动 PowerShell)
  - `src/update.rs`：版本更新检测 (GitHub Releases API + 镜像、WinHTTP、toast 去重)；检测循环逐次 catch_unwind
  - `build.rs`：把 icons 下的 ICO 嵌入 PE 资源 (桌面 exe 图标)
  - `tests/doc_language.rs`：扫描文档与源码注释，禁止过程性表述与历史依据用语
  - `icons/`：DeepSeekHarness-WhaleGirl.ico (256x256，唯一图标源)
  - `Cargo.toml`：bin 名 `DshLauncher`，release 带 lto+strip

## 路径书写规范 (全项目强制)

- Windows 路径在项目注释、README/AGENTS 等文档描述中一律使用正斜杠 `/`，不写反斜杠 `\`。
- 示例：`%USERPROFILE%/.dsh`、`%USERPROFILE%/.dsh/launcher.log`、`C:/Program Files/...`、`D:/Scoop/persist/rustup/.cargo`。
- 代码字符串、PowerShell/正则转义等运行所需的字面反斜杠除外；这些反斜杠按语法要求保留，不按“文档路径”处理。

## 防回归红线 (最高优先级，违反即返工)

### 为什么这些会反复出现
- 之前只把修复写成“经验记录”，没有写成“禁止项”；后人重构时又退回旧方案。
- 允许“临时 PowerShell/脚本兜底”存在，结果兜底慢慢变成长期实现。
- 修复后缺少自动化检查，CI 无法阻止同类代码再次合入。

### 红线清单
1. **应用运行时必须是纯 Rust/Win32，禁止任何 PowerShell 代理**：
   - 禁止 `Command::new("powershell.exe")`、`-EncodedCommand`。
   - 更新通知必须走 `src/toast.rs` 的 WinRT + 注册表 API。
   - 停止 dsh 必须走 `TerminateJobObject`，外部残留用 `GetExtendedTcpTable` + Toolhelp + `TerminateProcess`。
   - 唯一允许的运行时外部命令：启动 dsh 的 `cmd.exe /c npx ...`。
2. **测试必须覆盖真实杀伤路径**：
   - 端口查询必须用真实 `TcpListener` 验证。
   - 进程清理必须真实 spawn 子进程后终止。
   - 禁止只测“无目标时不出错”。
3. **winit 主线程禁止任何可能阻塞的操作 (>10ms)**：
   - `port_ready()`、sleep、网络、进程管理、日志文件读改写都放到后台线程。
   - “打开”菜单用后台探测 + `OpenProbeDone` 事件；`ShellExecuteW` 打开页面/配置目录也在后台线程执行。
4. **CI 发布判定只比源码，不比二进制 hash**：
   - 使用 `git diff -b --name-only <上一发布tag>..HEAD`。
   - 只统计真正参与构建的文件：`Cargo.toml` / `Cargo.lock` / `build.rs` / `src/**` / `icons/DeepSeekHarness-WhaleGirl.ico`。
   - 在 diff 基础上过滤纯注释行；注释/空白变化不发布，文档与 PNG 图片变化不发布。
   - 不要重新引入“下载上一 Release → 编译 → SHA256 对比”。
5. **更新去重状态只存在于 launcher.log 的 3 天窗口内**：
   - 去重依据是最近一条仍在保留窗口内的“更新通知成功”日志行；“更新检测成功”行只记录检查结果，不承担去重。
   - 通知失败只写 FAIL 日志，不写通知成功记录，因此进程重启后同一版本仍会重试。
   - 不得重新引入 `update-notified.txt` 之类的独立状态文件。
   - 不得永久保留任何日志行绕过 3 天清理。
6. **dsh 启动必须先挂 Job，再检查 quitting**：
   - cmd 以 `CREATE_SUSPENDED` 创建，挂入全局 Job 并恢复主线程后才开始执行，因此不存在挂接前派生 npx/node 的窗口。
   - 挂接或恢复失败时清理整棵进程树；恢复后才检查 quitting，收到退出请求用 `TerminateJobObject` 终止。
7. **日志清理必须流式处理，且只能在日志管理线程内执行**：
   - 禁止对 `launcher.log` 使用 `read_to_string` 后整体重写。
   - 必须逐行读、写临时文件、`rename` 替换。
   - 单文件超过 10 MiB 后按行截断到 8 MiB 以内，同样走临时文件 + `rename`，只在日志线程执行。
   - 其它线程只向日志线程消息队列投递 Write/Read/Flush，不在调用线程做文件 I/O。
8. **dsh 输出必须保留未完成 UTF-8 尾字节**：
   - 无换行输出跨块时，不能用 `from_utf8_lossy` 直接丢弃多字节字符的剩余字节。
9. **任何实现变化必须同步修改 AGENTS.md / README.md**：
   - 文档还写着旧实现 (如 PowerShell、状态文件、hash 发布) 视为未完成。
10. **更新日志、注释与文档只描述当前状态**：
   - 提交信息、`src/` 注释、`build.rs` 注释、README.md、CI 注释不得写过程描述或历史依据，例如“不再/曾/此前/原先/改为/移除/新增/修复/本次/已实测”。
   - 不得写无出处的笼统说法，例如“并按审查意见加固”。
   - AGENTS.md 是项目规范与踩坑记录的锚点，是唯一允许保留上述过程性内容、历史约束与经验记录的文件。

### 自动防线 (已经落地)
- `runtime_source_has_no_shell_proxies`：扫描源码，禁止 `Command::new("powershell.exe")` / `netstat.exe` / `taskkill.exe` 与 `-EncodedCommand`。
- `docs_and_source_have_no_process_language`：扫描 README/build.rs/src/CI/Cargo.toml，禁止过程性表述与历史依据用语 (AGENTS.md 除外)。
- `listener_pids_finds_current_process`：真实 `TcpListener` 验证端口 → PID。
- `process_full_image_name_finds_current_process`：验证 `QueryFullProcessImageNameW` 完整镜像路径查询。
- `process_command_line_reads_dsh_marker_from_child_process`：真实 spawn 子进程并读取命令行中的 `@deepseek-ai/dsh` 标记。
- `kill_process_tree_terminates_child_process`：真实 spawn 子进程并终止。
- `resume_process_main_thread_starts_suspended_process`：真实 spawn 挂起进程并恢复主线程。
- `truncate_log_from_head_*`：验证按大小截断保留尾部完整行。
- `take_flushable_partial_bytes_keeps_incomplete_utf8_tail`：验证 UTF-8 尾字节保留。
- CI：`cargo fmt --check` + `cargo clippy --locked --all-targets -- -D warnings` + `cargo test --locked`。

### 修改后必检清单
- [ ] `grep -R "powershell.exe\|EncodedCommand\|netstat.exe\|taskkill.exe" src Cargo.toml build.rs` 无运行时调用 (历史注释除外)。
- [ ] `grep -R -n -E '不再|曾|此前|原先|改为|移除|新增|修复|经验|本次|已实测|按审查' src build.rs README.md Cargo.toml .github/workflows/ci.yml` 无命中 (AGENTS.md 除外)。
- [ ] 提交信息只描述当前状态，不写过程描述与无出处说法。
- [ ] `cargo fmt --check`、`cargo clippy --locked --all-targets -- -D warnings`、`cargo test --locked`、`cargo build --locked` 全部通过。
- [ ] 没有在主线程新增 `port_ready()` / sleep / 网络 / 进程管理调用。
- [ ] Job 挂接先于 quitting 检查；失败路径清理整棵进程树。
- [ ] 日志清理是流式的；没有新的隐藏状态文件。
- [ ] CI 发布判定仍是源码 diff，没有退回二进制 hash。
- [ ] AGENTS.md 与 README.md 已同步本次实现变化。

## 架构 (守护模型)

### 状态标志 (Arc<AtomicBool>，防重入)
- `starting`：是否有启动流程进行中；`swap(true)` 抢锁，流程结束由 `StartingGuard` (RAII) 释放；正常结束先释放再发 `AnimationDone`，主线程读到实时状态
- `pending_open`：启动期间用户请求"打开页面"的待办；`AnimationDone` 就绪时消费；`pending_from_restart` 标记来源，重启失败时清除待办 (打开来源保留)
- `quitting`：退出请求；watchdog 与启动流程各阶段检查后立即放弃

### 守护线程 (App::spawn_watchdog)
- 每 2 秒探测 dsh 端口；连续拉起失败 3 次后放慢到 30 秒
- 单次检查在 catch_unwind 内执行：异常时写日志、重读 `was_ready`、置动画开关为运行，并在 2 秒后继续，守护线程本身不退
- dsh 未运行且无启动流程 → `spawn_startup_flow(proxy, starting, quitting, anim_running)` (含清理残留)；启动流程线程用 `thread::Builder::spawn` 创建，创建失败时立即释放 `starting`
- 启动器启动时 dsh 已在运行 (端口连通) → 直接复用，不杀不重启

### 启动流程 (spawn_startup_flow)
- **Job Object + KILL_ON_JOB_CLOSE (核心设计)**：start_harness 以 `CREATE_SUSPENDED` 创建 cmd，挂入全局 Job 后恢复主线程，dsh 进程树 (cmd→npx→node) 全程受 Job 管理；DshLauncher 退出/崩溃/被强杀时 Job 句柄随进程关闭，Windows 自动终止 dsh —— 从设计上保证无孤儿残留，常态启动/退出**不需要清理流程**
- 启动前仅当 `port_occupied()` (bind 探测失败 = 端口被外部进程占，毫秒级) 才 `stop_harness()` (TerminateJobObject 秒杀 + GetExtendedTcpTable/Toolhelp 纯 Win32 兜底)
- `start_harness()` (仅一次) → 轮询端口就绪 (500ms 间隔)；超时以 dsh 输出活动为心跳：连续 120 秒无任何输出才结束等待，持续有输出不判超时，期间每 5 秒通过 `StartupProgress` 事件更新 tooltip
- 动画由独立动画线程驱动 (见"动画策略")，flow 不负责换帧，也不手动停动画
- 结束：`starting` 释放 → 发 `AnimationDone { ready }` → 主线程按 `pending_open` 待办打开页面 (重启来源的待办在失败时清除)；无输出超时会先 `stop_harness()` 清理本次启动树，避免 watchdog 重试时叠加进程
- 停止 dsh = `TerminateJobObject` (毫秒级)；外部残留兜底用 `GetExtendedTcpTable` 找监听 PID + Toolhelp 快照递归 `TerminateProcess`，全程纯 Win32

### 事件流
- 菜单/托盘事件：`MenuEvent::set_event_handler` / `TrayIconEvent::set_event_handler` → `EventLoopProxy::send_event` → `ApplicationHandler::user_event`
- 自定义事件：`Menu` / `Tray` / `AnimationTick` (换一帧) / `AnimationStop` (开关下降沿) / `AnimationDone { ready }` / `TooltipUpdate { ready }` / `OpenProbeDone { ready }` / `StartupProgress { elapsed_secs, output_active }`

### 菜单行为
- 打开：先在后台线程探测端口 (`OpenProbeDone` 回主线程)；就绪则直接 `open_page()`，未就绪则保留 `pending_open=true` + `set_anim(true)`，由 watchdog/flow 就绪后消费待办
- 配置：`open_config_dir()` (见下)
- 重启：**仅当 dsh 可连接且无启动流程时可点击** (dsh 未运行/启动中/重启中均为禁用)；点击瞬间禁用菜单 + `set_anim(true)` (扫描灯立即流动) + `pending_open=true` + `thread::spawn(|| stop_harness())` (**必须异步**：动画换帧依赖主线程事件循环，同步执行带 sleep 的 stop 会阻塞主线程导致动画延迟出现)，watchdog 拉起，watchdog 探测到就绪后停动画，flow 就绪后恢复菜单并打开页面
- 退出：`quitting=true` → 隐藏托盘图标 (即时反馈) → 关闭动画开关 → **提前 `CloseHandle` 释放单例互斥体** (新实例可立即启动) → `event_loop.exit()`；`stop_harness()` (Job 秒杀 + 外部残留兜底) 在 `run_app` 返回后执行，避免阻塞事件循环 (KILL_ON_JOB_CLOSE 兜底崩溃/异常退出场景)
  - 背景：若不提前释放，图标隐藏到进程退出的窗口期内新实例会被单例拒绝启动 (用户会以为程序没响应)
- 左键单击无功能 (`with_menu_on_left_click(false)`)；左键双击等同"打开"

### 托盘动画 (16 帧，150ms/帧，扫描仪灯管定稿)
- 动画只有一个 `AtomicBool` 开关：重复置 true 只刷新标志；只有 watchdog 确认 dsh 可连接 (或“打开”后台探测确认就绪且无重启流程) 才置 false
- 动画线程观察 true→false 下降沿发送 `AnimationStop`；主线程处理时若开关又被置 true，则忽略迟到停止
- 图标大小不变；2px 不透明全白灯管 + 单侧 6px 线性衰减半透明灯光
- 灯管中心用连续坐标在 1.0~31.0 三角波来回扫动，逐列按像素中心计算；左右边界按画布裁切，灯管始终保持 2px；逐列 overlay 叠加 (透明区域也能被照亮)
- 图标裁剪：以 PNG (984x984) 中心裁剪 760x760 的比例 (≈0.7724) 等比应用到 ICO 源 (256 → 中心 198)
- 生成：`load_tray_icons()` 构建时一次性生成 (裁剪 + 逐帧绘制灯管/灯光列)
- 默认图标与动画帧都从内嵌 ICO 解码 (`include_bytes!`，32x32 RGBA)

## 关键实现细节 (踩过的坑)

### tray-icon 0.19 / winit 0.30 API 要点
- `set_icon` 参数是 `Option<Icon>`：`tray.set_icon(Some(icon))` (返回 Result，用 `let _ =`)
- 方法名是 `with_menu_on_left_click` (不是 `with_show_menu_on_left_click`)
- winit 0.30 用 `EventLoop::<T>::with_user_event().build()` + `run_app(&mut app)`；`run` 已废弃
- `ApplicationHandler` 的 `window_event` 无默认实现，必须显式实现 (空实现即可)
- `MenuEvent::id()` 返回 `&MenuId`，比较用 `ev.id() == &self.open_id`

### windows-sys 0.59 要点
- 项目统一用 `to_wide()` 生成带终止符的 UTF-16 `Vec<u16>`，传参用 `.as_ptr()`；`CreateMutexW` 等 Win32 API 直接接收该指针
- `CreateMutexW` 被 `cfg(feature = "Win32_Security")` 门控；`ShellExecuteW` 需要 feature `Win32_UI_Shell`
- Cargo.toml features：`Win32_Foundation` + `Win32_System_Threading` + `Win32_Security` + `Win32_UI_Shell` + `Win32_System_JobObjects` + `Win32_System_SystemInformation` + `Win32_System_Registry` + `Win32_UI_WindowsAndMessaging` + `Win32_NetworkManagement_IpHelper` + `Win32_System_Diagnostics_ToolHelp` + `Win32_System_Diagnostics_Debug` + `Win32_Networking_WinSock` + `Win32_Networking_WinHttp` + `Wdk_System_Threading`
- `image` 依赖关闭默认特性，只启用 `ico` (内含 bmp/png 支持)，减小构建依赖面
- `ShellExecuteW` 返回 HINSTANCE，`as isize > 32` 表示成功 (打开 URL 无黑框，替代 cmd start)

### 纯 Win32 兜底清理 (dsh.rs)
- 更新 toast 已由 `src/toast.rs` 直接 WinRT 发送；停止 dsh 的兜底路径也已去掉 PowerShell/Base64
- 找监听 PID：`GetExtendedTcpTable(TCP_TABLE_OWNER_PID_ALL)`，IPv4 端口以网络字节序比较 (`u16::from_be`)
- 监听进程身份确认：Toolhelp 先取 exe 名，命中 node/cmd/dsh/npx 后用 `QueryFullProcessImageNameW` 取完整镜像路径，并用 `NtQueryInformationProcess(ProcessBasicInformation)` + PEB 读取命令行；node/npx/cmd 的命令行必须包含 `@deepseek-ai/dsh` (兼容 `/` 与 `\` 分隔符)，dsh.exe 按镜像名确认；名称命中但身份未确认的进程跳过，快照中查不到名称时仍保守清理；跨 32/64 位进程时不读命令行，按未确认处理
- 清理进程树：`CreateToolhelp32Snapshot` + `Process32First/Next` 建立父子关系 → 先子后父 `OpenProcess(PROCESS_TERMINATE)` + `TerminateProcess`；祖先链同样只终止通过身份确认的父进程
- 该路径由 `listener_pids_finds_current_process` 单测覆盖端口/PID 查找；`process_command_line_reads_dsh_marker_from_child_process` 真实 spawn 子进程覆盖命令行读取
- 端口 `DSHLAUNCHER_PORT` 环境变量可覆盖默认 3080

### dsh 启动与输出日志 (dsh.rs)
- start_harness 用 `Command::new("cmd.exe")` + `creation_flags(CREATE_NO_WINDOW | CREATE_SUSPENDED)` 创建 `cmd /c npx -y @deepseek-ai/dsh web`，挂入全局 Job 后用 Toolhelp 线程快照 `OpenThread + ResumeThread` 恢复主线程
- stdout/stderr 由读取线程分块写入 `~/.dsh/launcher.log` (时间标签 + `[ DSH]` 标记；按换行切行，无换行按缓冲块落盘，与启动器日志合并，随 3 天清理 + 10 MiB 大小上限)：npx/node 的全部输出落盘，启动卡顿/失败从此定位 (2026-08-18 曾两次 120 秒超时但无任何线索可查)
- 进程树为 cmd→npx→node：cmd 挂入 Job 后才恢复执行；挂接/恢复失败用 Toolhelp 快照递归清理整棵树，避免孤儿

### 打开资源管理器配置目录
- 直接用 `ShellExecuteW` 打开 `%USERPROFILE%/.dsh` (打开前 `create_dir_all` 确保存在)
- 曾用 PowerShell `Shell.Application` COM 遍历 `$shell.Windows()` 复用窗口，但隐藏 PowerShell 进程里 COM 枚举不稳定，导致点击「配置」不弹文件夹，已改为 ShellExecuteW

### build.rs (图标嵌入)
- 动态生成 .rc (含 ICO 绝对路径) → 定位 rc.exe (`RC_EXE` 环境变量 → PATH → Windows Kits → VS 标准固定路径精确匹配，不做全盘递归) → `rc /fo app.res` → `cargo:rustc-link-arg` 传 .res
- 本机 rc.exe：`C:/Program Files (x86)/Windows Kits/10/bin/<ver>/x64/rc.exe`

## 版本号与 CI 发布

- exe 版本号格式 `YY.MM.DD.NN` (年.月.日.当日第几次发布，各段固定两位补零，如 26.08.05.01)：CI 发布时经环境变量 `DSHLAUNCHER_VERSION` 传入 build.rs 嵌入 FILEVERSION；本地构建在 build.rs 重跑时取当天本地日期 + 0 (Cargo.toml version 不参与 exe 版本)
- build.rs 声明 `src/`、`Cargo.toml`、`Cargo.lock`、ICO 图标与 `DSHLAUNCHER_VERSION` 为重跑输入：源码变化 → build.rs 重跑 → 取当天日期；只跨天而源码未变 → 不重跑 → 重复构建保持旧版本，符合 CI“源码无实质变化不发布”的逻辑；`/Brepro` 用于产物可复现性
- `DSHLAUNCHER_VERSION` 设置后必须是四段数值且每段 ≤ 65535，非法值直接终止构建，避免静默回退日期版本
- CI 发布 (push 到 main 且涉及编译文件时触发，纯文档改动不触发)：
  1. 用 `git diff -b --name-only <上一发布tag>..HEAD` 获取有非缩进变化的文件
  2. 只统计真正参与构建的文件 `Cargo.toml` / `Cargo.lock` / `build.rs` / `src/**` / `icons/DeepSeekHarness-WhaleGirl.ico`；Rust/TOML 的纯注释行变化过滤后不发布，PNG 与文档变化不发布
  3. 计算当日发布号 NN (北京时间，当日已发布数 + 1)，以 `vYY.MM.DD.NN` 为 tag 与标题发布 Release，正文为上一发布以来 `git log` 的提交列表
- 发布由 `gh release create` 完成 (自动打 tag)，附带 exe 与 SHA256 校验文件；workflow 需要 `permissions: contents: write`，release job 用 concurrency 串行化同分支发布

## 更新检测

- 检测源：GitHub Releases API 直连为主 (`https://api.github.com/repos/NekoPawClub/DshLauncher/releases/latest`)，失败依次尝试 gh-proxy 镜像前缀 (ghproxy.net、ghfast.top)；环境变量 `DSHLAUNCHER_UPDATE_MIRROR` 可自定义镜像前缀 (逗号分隔)，置于内置候选之前；环境变量 `DSHLAUNCHER_UPDATE_DISABLE=1/true/yes/on` 可完全禁用检测 (开发/测试用)
- 实现：WinHTTP (windows-sys `Win32_Networking_WinHttp`)，零第三方依赖，自动走系统代理；单请求超时 解析 3s/连接 5s/发送 8s/接收 8s；HTTPS 限定 TLS 1.2/1.3，只接受 2xx 响应
- 版本比较：`YY.MM.DD.NN` 按 . 分段转数值逐段比较 (段长不固定，字符串字典序会误判)；本地版本由 build.rs 经 rustc-env 注入 (`env!("DSH_LAUNCHER_VERSION")`)
- 检测节奏：启动即首查一次，之后每 1 小时复查；检测线程直接发 Windows 通知 (toast)，不占用主线程
- 结果写日志节奏：检测成功写 `更新检测成功：远端 vX (本地 vY)`；进程启动写一次，跨凌晨 4 点日志日写一次，同一日志日内仅远端出现更新版本时增写，避免每小时重复刷日志
- 通知去重：发现更新后由 `src/toast.rs` 直接通过 WinRT 发系统通知 (点击通知用默认浏览器打开下载页面，activationType=protocol)；发送前在 HKCU 注册自有 AUMID (NekoPawClub.DshLauncher：显示名 DshLauncher + exe 内嵌图标)，通知中心显示为 DshLauncher；发送成功后写 `更新通知成功：远端 vX`，发送失败只写 FAIL 并在本进程内按小时重试；去重依据为 launcher.log 中最近 3 天保留窗口内最后一条“更新通知成功”记录的版本 (检测/通知行都按普通日志清理，不永久保留)，同一版本在保留窗口内不重复提示，出现更新的版本后再提示；检测失败静默 (仅日志)
- 远端版本来自 API 响应的 tag_name 字段，手工解析 (零 serde)

## 构建

### 用户机器 (正常网络)
- 直接 `cargo build --release`，直连官方源，无需任何代理
- 产物：`target/release/DshLauncher.exe` (已内嵌图标)
- 若 exe 正被运行占用，链接/复制会失败——先退出托盘实例
- 验证构建只需 `cargo build` (debug)，不要碰用户正在运行的 release exe

### 沙箱环境 (开发代理会话)
- 沙箱 schannel TLS 被网关阻断 (node/OpenSSL 可通，cargo/curl/PowerShell 不可通)——与文件沙箱权限无关
- 沙箱不可写 `D:/Scoop/persist/rustup/.cargo` → 构建前设 `$env:CARGO_HOME = "$env:TEMP/dsh-cargo-home"`
- 需临时重建镜像代理 (验证后删除，勿提交)：
  1. 写 `.cargo/config.toml`：`replace-with = "local-mirror"`，`registry = "sparse+http://127.0.0.1:8081/"`
  2. 写 `scripts/sparse-mirror.mjs` (node 代理，转发 index.crates.io / static.crates.io)
  3. `Start-Process node <script> -WindowStyle Hidden` 后 `cargo build`
- 镜像代理路径规则 (关键！)：`.crate` 结尾或 `/download` 结尾 → 下载 (static.crates.io/crates/...)；`/dl/` 开头的其他路径 → 索引原样转发 (crate 前缀恰为 `dl/`，如 dlib → `dl/ib/dlib`，绝不能剥前缀)
- crates.io 索引条目没有 `dl` 字段；下载 URL 由 config.json 返回的 `dl` 前缀拼接 (`/dl/{name}/{ver}/download` 与 `/dl/{name}/{name}-{ver}.crate` 两种格式)

## 测试方法 (红线：绝不触碰用户的 DshLauncher 与真实 dsh)

真实 dsh (监听 127.0.0.1:3080 的 node) 与用户正在运行的 DshLauncher 实例都是当前环境运行依赖。

**绝对禁止：**
- `Stop-Process -Name DshLauncher` —— 会杀掉用户的守护实例。**真实事故**：用户点「重启」后 dsh 已被终结、正等待守护拉起，此时测试再杀守护 → dsh 永久失保被关闭
- `Stop-Process -Name node` —— 会杀掉真实 dsh
- 在用户实例运行时直接启动测试实例 (单实例互斥体会让测试实例退出)
- **测试脚本内的清理动作同样遵守**：写完脚本先检查其中是否含 `Stop-Process -Name` 之类的宽泛匹配 (曾写出含 `Stop-Process -Name node` 的测试脚本，发现后立即删除)

**安全测试组合 (三要素)：**
1. `DSHLAUNCHER_PORT=39999` (隔离端口)
2. `DSHLAUNCHER_INSTANCE=test` (互斥体后缀隔离，测试实例与用户实例共存)
3. `DSHLAUNCHER_UPDATE_DISABLE=1` (避免测试实例弹真实更新 toast)
4. `PATH` 前置假 npx (`scripts/test-bin/npx.cmd`：`ping -n 60 127.0.0.1 >nul` 模拟 60 秒启动)

**清理规则：** 一律 `Start-Process -PassThru` 拿 PID 按 `Stop-Process -Id` 清理；端口进程用 `netstat -ano | findstr :39999` 定位 PID。测试后必须确认用户实例仍存活。

**验证点：** 进程存活 (托盘创建)、动画期间稳定、同 INSTANCE 下单例 (进程数 1)、崩溃后 watchdog 自动拉起 (39999 端口恢复)。

## 动画策略 (全局动画控制器)

- 动画由独立动画线程驱动：`anim_running` 为 true 时每 150ms 发 `AnimationTick` 换帧；线程同时观察 true→false 下降沿，只在该边沿发 `AnimationStop`
- `set_anim(running)` 只写 `AtomicBool`：重复置 true 不改变运行状态，也不直接发事件
- **程序启动即让扫描灯流动**；watchdog 首轮探测 (`was_ready=None` 强制触发) 就绪后停止
- watchdog 记录 `was_ready` 状态：只有 dsh 可连接 (就绪) 才置 false；“打开”后台探测确认就绪且无重启流程时也可置 false；其它路径不手动停动画
- 重启菜单：点击瞬间禁用重启项 + `set_anim(true)` + 异步 stop；watchdog 探测到新 dsh 就绪后停动画并恢复重启项，flow 就绪后打开页面
- **扫描仪灯管动画 (定稿)**：图标大小不变；2px 不透明全白灯管 + 单侧 6px 线性衰减半透明灯光；灯管中心用连续坐标在 1.0~31.0 三角波扫动，逐列按像素中心计算，左右边界灯管被画布自然裁切且保持 2px；逐列 overlay 叠加 (透明区域也能被照亮)
- 曾尝试并被否掉的方案：呼吸缩放 (1.0~1.5 放大呼吸)、底部滚动条 (白色 1/8~1/4 高度)、亮度高亮——需要时按 git 历史恢复

## 纯 Rust 化实现细节 (本次修复经验)

- **CI 发布判定不要比二进制 hash，要比源码 diff**：用 `git diff -b --name-only <上一发布tag>..HEAD` 找非缩进变化文件；只统计 `Cargo.toml`/`Cargo.lock`/`build.rs`/`src/**`/`icons/DeepSeekHarness-WhaleGirl.ico`，并过滤 Rust/TOML 纯注释行。工具链升级不会影响该判定。
- **更新通知走 Rust/WinRT，不要经 PowerShell 代理**：`windows` crate (`Data_Xml_Dom` + `UI_Notifications`) 直接创建 `XmlDocument`/`ToastNotification`，`ToastNotificationManager::CreateToastNotifierWithId` 发送；AUMID 用 `windows-sys` 注册表 API 写 HKCU。exe 图标路径用百分号编码转 `file:///` URI。
- **停止 dsh 的兜底也必须是纯 Win32**：`GetExtendedTcpTable` 找监听 PID，Toolhelp 快照构建进程树，先子后父 `TerminateProcess`；快照同时读取 exe 名称，只终止 `node.exe`/`cmd.exe`/`dsh.exe`/`npx.exe` 等已知 dsh 进程，避免端口误配时误杀无关服务；不要再回到 `netstat`/`taskkill`/PowerShell。
- **Job 挂接要先于 cmd 恢复执行与 quitting 检查**：cmd 以 `CREATE_SUSPENDED` 创建，先挂 Job 再 `ResumeThread`；任何失败路径清理整棵树，恢复后收到退出请求用 `TerminateJobObject`。
- **主线程不要同步 `port_ready()`**：`connect_timeout` 最长 500ms 会卡动画；“打开”菜单改为后台探测 + `OpenProbeDone` 事件回主线程。
- **start_harness 的 Err 不能吞掉**：否则启动失败会误报“无输出超时”；要区分“启动失败”和“等待就绪无输出超时”。
- **启动超时以输出活动为心跳**：连续 120 秒无 dsh 输出才结束等待；只要有持续输出就不超时，并通过 tooltip 显示已等待时长/是否仍在输出。
- **toast 成功后再写“已发送”日志**：发送失败记录具体错误并在本进程按小时重试，避免假成功且不再重试。
- **日志清理要流式，且必须在日志管理线程**：dsh 输出合并后日志可能很大，`read_to_string` 全量清理会阻塞所有写入；改为逐行读 + 临时文件 + `rename` 替换；写日志只投递消息队列，清理不会卡主线程或 dsh 输出读取线程。
- **dsh 输出按块落盘要保留 UTF-8 尾字节**：无换行输出跨 16 KiB 块时，不能直接 `from_utf8_lossy` 截断多字节字符；保留 `error_len() == None` 的尾字节，非法字节才 lossy 落盘。
- **DSHLAUNCHER_INSTANCE 要统一清洗**：trim + 只保留 `[A-Za-z0-9_-]`，日志文件名和单实例互斥体共用同一结果，防止路径穿越和命名不一致。
- **rc.exe 兜底查找要按主机架构选择**：Windows Kits 与 VS 标准路径结果都要优先 x64/x86/arm64 中匹配 `std::env::consts::ARCH` 的候选；VS 只按 `<root>/<year>/<edition>/VC/Tools/MSVC/<ver>/bin/Host<host>/<target>/rc.exe` 精确拼接，禁止盲目递归。
- **自定义更新镜像按 URL scheme 决定 `WINHTTP_FLAG_SECURE`**：不要固定 HTTPS flag，否则 `DSHLAUNCHER_UPDATE_MIRROR=http://...` 无法工作。

## 调试经验 (踩坑实录)

- **测试掩盖静默失效**：杀 dsh 的正则被 JS 转义破坏后，测试从未暴露——因为测试场景 (39999 无监听者) 下 stop 无操作也"通过"。教训：测试要覆盖"真实杀伤"路径 (先起假监听进程再杀)，不能只测"无目标时不出错"。**当前已固化**：`listener_pids_finds_current_process` 覆盖端口→PID 的真实查询路径；`kill_process_tree_terminates_child_process` 覆盖真实 TerminateProcess；进程树清理走 Toolhelp 快照，不再依赖任何脚本正则
- **验证写入内容用 grep 看实际行**，不要用 PowerShell 正则/Contains 验证 (验证命令自身的转义和单引号不转义规则会误导；grep 直接显示文件真实字节最可靠)
- **假进程用脚本文件而非内联参数**：`Start-Process node -ArgumentList '-e','require(...)'` 的参数拼接经常失败 (假 dsh 起不来、断言全废)；用 write 工具写 `fake-dsh.js`/`npx.cmd` 文件再启动，稳定可靠
- **工具调用被中断后**：先确认外部状态 (文件/进程/端口) 再决定重试——中断的调用结果未知，幂等操作可重试，有副作用的先验证
- **用户机器与沙箱网络差异**：沙箱 schannel 被网关阻断 (需临时镜像)，用户机器直连正常。构建前先判断在哪构建：沙箱内先查 8081 代理是否还在跑 (在跑直接复用，不用重建镜像)
- **动画需求迭代要点** (用户偏好)：不要减淡只高亮；滚动条要"滚动游走"不要"进度填充"；程序启动即流动、就绪才停；重启点击瞬间起动画
- **主线程事件循环禁止耗时操作** (用户发现的实际问题)：winit 事件循环 (user_event 处理) 里同步执行任何含 sleep/网络/进程管理的操作，都会阻塞动画换帧与所有事件处理。真实案例：重启菜单把 `stop_harness()` 从 `thread::spawn` 改为同步调用后，扫描灯延迟出现 (Job 秒杀虽快但 stop 内含 300ms sleep + 端口检查)。规则：**凡是可能耗时 (>10ms) 的操作一律放后台线程，事件循环只做状态切换与 UI 更新**

## 工具调用经验 (避免反复出错)

- **转义问题的第一原则：不要对抗转义，绕过它**。凡是命令里出现引号/反斜杠/反引号/多行/变量嵌套，立即改用以下已验证手法 (按优先级)：
  1. **write 工具写脚本文件再执行** (本会话成功率最高)：复杂命令写成 `.ps1` 后 `powershell -NoProfile -ExecutionPolicy Bypass -File xxx.ps1`，或写成 `.cmd`/`.mjs`/`.js` 直接运行；假 npx、假 dsh、测试脚本、镜像代理全部走此方案
  2. **开发/测试脚本调 PowerShell 时可用 `-EncodedCommand`** (历史做法，应用运行时代码已全部移除 PowerShell)：脚本 UTF-16LE → Base64 后传入，任何特殊字符都免转义
  3. **写简单文件用 here-string**：PowerShell `@'...'@` (单引号形式，不展开变量) 配合 `Set-Content`，避免 Set-Content 的引号嵌套
  4. **TS 内联写 Rust/PowerShell 源码**：反斜杠一律双写 `\\` (见下条"JS 吃掉反斜杠")，写完必须 grep 验证；更复杂的含正则内容直接拆成独立文件维护
  5. **避免反引号**：PowerShell 换行 `\`n` 在 TS 模板字符串里要写成 `\\\`n` 极易错——用 here-string 或脚本文件替代
  6. **Start-Process 不要内联复杂参数**：`-ArgumentList '-e','require(...)'` 拼接不可靠，把逻辑写进脚本文件再 `node 文件`/`cmd /c 文件` 启动
- **read-before-write/edit**：文件工具要求先 read 才能写/编辑；文件被外部删除后 write 会报 `file no longer exists`——先确认存在 (PowerShell 建目录/写文件亦可)
- **write 不自动建目录**：目标目录不存在时先 `New-Item -ItemType Directory -Force`
- **转义地狱**：run_code 的 TS 模板字符串里内联 PowerShell 极易转义出错 (反引号、引号、\\)；复杂内容一律用 write 工具写成脚本文件再执行
- **JS 会吃掉无效转义的反斜杠 (本项目的重大事故根源)**：在 TS 模板字符串中写 Rust/PowerShell 源码时，`\s`、`\d` 等无效转义序列会被 JS 解析为 `s`、`d` (反斜杠被丢弃)！本项目因此导致 stop 脚本正则 `"TCP\s+\S*:"` 静默变成 `"TCPs+S*:"`，杀 dsh 功能长期未真正生效。规则：**凡是要写入文件的反斜杠，TS 里一律双写 `\\`** (正则 `\s` 写 `\\s`，PowerShell 单引号 `'\\'` 写 `'\\\\'`，Rust 字符串 `\` 写 `\\\\`)，写完必须用 grep 看实际行字节确认，不要依赖肉眼或 PowerShell 正则验证 (验证命令自身的转义同样会骗人)
- **PowerShell 5.1 编码**：无 BOM 的 UTF-8 脚本中文字符串会按 GBK 乱码导致解析错误——脚本输出用英文，或保存为 UTF-8 BOM
- **沙箱查询限制**：`Get-NetTCPConnection` 常返回空 (用 `netstat -ano`)；WMI `CommandLine` 可能为空；`Get-CimInstance` 可能被拒 (用 netstat 代替)
- **后台任务**：长命令用 `run_in_background: true`，用 `job_output({wait: true})` 阻塞等待
- **超时按任务实际需要设置，不要一律 600s** (用户明确批评过)：增量编译 60s、首次编译 180s、运行测试 100~200s、一般命令 30s；时间到了还 running 就再等一轮，而不是给超长超时
- **Start-Process 长生命周期进程必须重定向输出** (用户发现的关键坑)：在后台 job 内 `Start-Process node ...` 启动代理等常驻进程时，子进程会继承 job 管道的 stdout/stderr 句柄——cargo 编译完成、pwsh 脚本结束后 job 仍显示 running (管道不 EOF)，直到超时。修复：`-RedirectStandardOutput $logOut -RedirectStandardError $logErr` 重定向到日志文件。任何 Start-Process 常驻进程 (代理、假 dsh 等) 都要加
- **构建命令禁用 `Select-Object -Last N`** (用户发现的关键坑)：它必须等命令完全结束才输出，导致编译失败信息"到超时才出现"。正确写法：`cargo build 2>&1` 流式输出 + 末尾 `Write-Host ("[exit-code: " + $LASTEXITCODE + "]")`。另注意：build.rs 失败后 cargo 会打印 "build failed, waiting for other jobs" 并继续等并行 crate 编译完成 (可能 10~30s) 才退出——这是 cargo 行为，流式输出后失败信息立即可见，不必等 job 结束
- **代理等常驻进程用独立后台 job 启动** (脚本立即结束、代理常驻)，构建用另一个 job，两者生命周期解耦；job_kill 构建 job 会连带杀掉其启动的代理 (进程树)
- **git 状态**：`.cargo/`、根目录 exe 副本、test-bin 均不应入库；改动后检查 `git status` 是否干净
