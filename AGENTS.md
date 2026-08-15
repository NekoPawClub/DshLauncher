# AGENTS.md — DshLauncher 项目指南

DeepSeek Harness（dsh）的 Windows 系统托盘守护启动器，Rust 编写。
本文件汇总项目架构、编译经验与工具调用注意事项，避免重复踩坑。

## 项目概述

- 作用：常驻系统托盘，作为 dsh 的守护程序（watchdog），保证 dsh 持续运行
- 启动命令：`npx @deepseek-ai/dsh web`（后台隐藏窗口）
- 技术栈：Rust 1.97 / tray-icon 0.19 / winit 0.30 / image 0.25 / windows-sys 0.59 / base64 0.22
- 目录结构：
  - `src/main.rs`：托盘、菜单、watchdog、启动流程、动画
  - `src/dsh.rs`：dsh 进程控制（启动/停止/端口探测）、ShellExecuteW、COM 脚本、单实例
  - `build.rs`：把 Icons 下的 ICO 嵌入 PE 资源（桌面 exe 图标）
  - `Icons/`：DeepSeekHarness-WhaleGirl.ico（256x256，唯一图标源）
  - `Cargo.toml`：bin 名 `DshLauncher`，release 带 lto+strip

## 架构（守护模型）

### 状态标志（Arc<AtomicBool>，防重入）
- `starting`：是否有启动流程进行中；`swap(true)` 抢锁，流程结束 `store(false)`
- `pending_open`：启动期间用户请求"打开页面"的待办；`AnimationDone` 时 `swap(false)` 消费
- `quitting`：退出请求；watchdog 与启动流程各阶段检查后立即放弃

### 守护线程（App::spawn_watchdog）
- 每 2 秒探测 dsh 端口；连续拉起失败 3 次后放慢到 30 秒
- dsh 未运行且无启动流程 → `spawn_startup_flow(restart=true, open=false)`（含清理残留）
- 启动器启动时 dsh 已在运行（端口连通）→ 直接复用，不杀不重启

### 启动流程（spawn_startup_flow）
- restart=true：`stop_harness_async()`（异步发起，不阻塞）→ 主循环 `try_wait` 检测清理完成
- 清理完成后 `start_harness()`（仅尝试一次）→ 轮询端口就绪（500ms 间隔，最长 120s）
- 动画由独立动画线程驱动（见"动画策略"），flow 不负责换帧
- 结束：`starting=false` → 就绪时停动画 → 发 `AnimationDone` 事件 → 主线程按 `open_when_ready || pending_open` 打开页面

### 事件流
- 菜单/托盘事件：`MenuEvent::set_event_handler` / `TrayIconEvent::set_event_handler` → `EventLoopProxy::send_event` → `ApplicationHandler::user_event`
- 自定义事件：`Menu` / `Tray` / `AnimationTick`（换一帧）/ `AnimationDone { ready, open_when_ready }`

### 菜单行为
- 打开：端口通 → `open_page()`；不通 → `pending_open=true` + 立即 `set_anim(true)`，watchdog 拉起后就绪自动打开
- 配置：`open_config_dir()`（见下）
- 重启：点击瞬间 `set_anim(true)`（滚动条立即流动）+ `pending_open=true` + 异步 `stop_harness()`，watchdog 拉起，flow 就绪后停动画并打开页面
- 退出：`quitting=true` → 多轮（最多 3 轮）`stop_harness` + 端口检查 → `event_loop.exit()`
- 左键单击无功能（`with_menu_on_left_click(false)`）；左键双击等同"打开"

### 托盘动画（16 帧，150ms/帧）
- 三合一：呼吸缩放（0.8~1.0）+ 高亮脉动（1.0~1.5，只增不减）+ 底部白色滚动条（高 1/8=4px，三角波游走，非进度填充）
- 图标裁剪：以 PNG（984x984）中心裁剪 760x760 的比例（≈0.7724）等比应用到 ICO 源（256 → 中心 198）
- 生成：`load_tray_icons()` 构建时一次性生成（裁剪 + 缩放居中 overlay + 亮度 + put_pixel 画条）
- 默认图标与动画帧都从内嵌 ICO 解码（`include_bytes!`，32x32 RGBA）

## 关键实现细节（踩过的坑）

### tray-icon 0.19 / winit 0.30 API 要点
- `set_icon` 参数是 `Option<Icon>`：`tray.set_icon(Some(icon))`（返回 Result，用 `let _ =`）
- 方法名是 `with_menu_on_left_click`（不是 `with_show_menu_on_left_click`）
- winit 0.30 用 `EventLoop::<T>::with_user_event().build()` + `run_app(&mut app)`；`run` 已废弃
- `ApplicationHandler` 的 `window_event` 无默认实现，必须显式实现（空实现即可）
- `MenuEvent::id()` 返回 `&MenuId`，比较用 `ev.id() == &self.open_id`

### windows-sys 0.59 要点
- `w!` 宏返回裸指针 `*const u16`（不是切片），直接传参：`CreateMutexW(null(), 1, name)`
- `CreateMutexW` 被 `cfg(feature = "Win32_Security")` 门控；`ShellExecuteW` 需要 feature `Win32_UI_Shell`
- Cargo.toml features：`Win32_Foundation` + `Win32_System_Threading` + `Win32_Security` + `Win32_UI_Shell`
- `ShellExecuteW` 返回 HINSTANCE，`as isize > 32` 表示成功（打开 URL 无黑框，替代 cmd start）

### PowerShell 调用（dsh.rs）
- 一律 `-EncodedCommand`（UTF-16LE + Base64 编码脚本），彻底避免命令行转义问题
- 启动进程加 `creation_flags(0x0800_0000)`（CREATE_NO_WINDOW）不弹控制台
- 杀 dsh：netstat 按端口找监听 PID → `taskkill /PID x /T /F`（杀进程树）；兜底匹配 node 命令行 `deepseek-ai[\\/]dsh`
- 调试开关 `DSHLAUNCHER_SAFE_TEST=1`：stop 仅按端口清理，跳过全局 node 匹配（开发机安全测试用）
- 端口 `DSHLAUNCHER_PORT` 环境变量可覆盖默认 3080

### 打开资源管理器复用窗口（VS Code 方案）
- PowerShell `Shell.Application` COM：遍历 `$shell.Windows()` 比较 `LocationURL` 前缀
- 命中 → `$w.Visible = $true` 激活既有窗口；未命中 → `$shell.Open($path)` 新建

### build.rs（图标嵌入）
- 动态生成 .rc（含 ICO 绝对路径）→ 定位 rc.exe（`RC_EXE` 环境变量 → PATH → Windows Kits → VS 递归）→ `rc /fo app.res` → `cargo:rustc-link-arg` 传 .res
- 本机 rc.exe：`C:\Program Files (x86)\Windows Kits\10\bin\<ver>\x64\rc.exe`

## 构建

### 用户机器（正常网络）
- 直接 `cargo build --release`，直连官方源，无需任何代理
- 产物：`target/release/DshLauncher.exe`（已内嵌图标）
- 若 exe 正被运行占用，链接/复制会失败——先退出托盘实例
- 验证构建只需 `cargo build`（debug），不要碰用户正在运行的 release exe

### 沙箱环境（开发代理会话）
- 沙箱 schannel TLS 被网关阻断（node/OpenSSL 可通，cargo/curl/PowerShell 不可通）——与文件沙箱权限无关
- 沙箱不可写 `D:\Scoop\persist\rustup\.cargo` → 构建前设 `$env:CARGO_HOME = "$env:TEMP\dsh-cargo-home"`
- 需临时重建镜像代理（验证后删除，勿提交）：
  1. 写 `.cargo/config.toml`：`replace-with = "local-mirror"`，`registry = "sparse+http://127.0.0.1:8081/"`
  2. 写 `scripts/sparse-mirror.mjs`（node 代理，转发 index.crates.io / static.crates.io）
  3. `Start-Process node <script> -WindowStyle Hidden` 后 `cargo build`
- 镜像代理路径规则（关键！）：`.crate` 结尾或 `/download` 结尾 → 下载（static.crates.io/crates/...）；`/dl/` 开头的其他路径 → 索引原样转发（crate 前缀恰为 `dl/`，如 dlib → `dl/ib/dlib`，绝不能剥前缀）
- crates.io 索引条目没有 `dl` 字段；下载 URL 由 config.json 返回的 `dl` 前缀拼接（`/dl/{name}/{ver}/download` 与 `/dl/{name}/{name}-{ver}.crate` 两种格式）

## 测试方法（红线：绝不触碰用户的 DshLauncher 与真实 dsh）

真实 dsh（监听 127.0.0.1:3080 的 node）与用户正在运行的 DshLauncher 实例都是当前环境运行依赖。

**绝对禁止：**
- `Stop-Process -Name DshLauncher` —— 会杀掉用户的守护实例。**真实事故**：用户点「重启」后 dsh 已被终结、正等待守护拉起，此时测试再杀守护 → dsh 永久失保被关闭
- `Stop-Process -Name node` —— 会杀掉真实 dsh
- 在用户实例运行时直接启动测试实例（单实例互斥体会让测试实例退出）
- **测试脚本内的清理动作同样遵守**：写完脚本先检查其中是否含 `Stop-Process -Name` 之类的宽泛匹配（曾写出含 `Stop-Process -Name node` 的测试脚本，发现后立即删除）

**安全测试组合（四要素）：**
1. `DSHLAUNCHER_PORT=39999`（隔离端口）
2. `DSHLAUNCHER_SAFE_TEST=1`（stop 仅按端口清理，跳过 node 匹配）
3. `DSHLAUNCHER_INSTANCE=test`（互斥体后缀隔离，测试实例与用户实例共存）
4. `PATH` 前置假 npx（`scripts/test-bin/npx.cmd`：`ping -n 60 127.0.0.1 >nul` 模拟 60 秒启动）

**清理规则：** 一律 `Start-Process -PassThru` 拿 PID 按 `Stop-Process -Id` 清理；端口进程用 `netstat -ano | findstr :39999` 定位 PID。测试后必须确认用户实例仍存活。

**验证点：** 进程存活（托盘创建）、动画期间稳定、同 INSTANCE 下单例（进程数 1）、崩溃后 watchdog 自动拉起（39999 端口恢复）。

## 动画策略（全局动画控制器）

- 动画由独立动画线程驱动：`anim_running` 为 true 时每 150ms 发 `AnimationTick` 换帧
- `set_anim(running)` 自由函数供任意线程调用：停止时发 `AnimationStop` 事件恢复默认图标
- **程序启动即让滚动条流动**；watchdog 首轮探测（`was_ready=None` 强制触发）就绪后停止
- watchdog 记录 `was_ready` 状态，dsh 就绪/失联状态变化时启停动画
- 重启菜单：点击瞬间 `set_anim(true)` + 异步 stop，watchdog 拉起，flow 就绪后停动画并打开页面
- 滚动条样式：白色（255,255,255），高 = 图标 1/8（32/8=4px），三角波来回游走（非进度填充）

## 调试经验（踩坑实录）

- **测试掩盖静默失效**：杀 dsh 的正则被 JS 转义破坏后，测试从未暴露——因为测试场景（39999 无监听者）下 stop 无操作也"通过"。教训：测试要覆盖"真实杀伤"路径（先起假监听进程再杀），不能只测"无目标时不出错"
- **验证写入内容用 grep 看实际行**，不要用 PowerShell 正则/Contains 验证（验证命令自身的转义和单引号不转义规则会误导；grep 直接显示文件真实字节最可靠）
- **假进程用脚本文件而非内联参数**：`Start-Process node -ArgumentList '-e','require(...)'` 的参数拼接经常失败（假 dsh 起不来、断言全废）；用 write 工具写 `fake-dsh.js`/`npx.cmd` 文件再启动，稳定可靠
- **工具调用被中断后**：先确认外部状态（文件/进程/端口）再决定重试——中断的调用结果未知，幂等操作可重试，有副作用的先验证
- **用户机器与沙箱网络差异**：沙箱 schannel 被网关阻断（需临时镜像），用户机器直连正常。构建前先判断在哪构建：沙箱内先查 8081 代理是否还在跑（在跑直接复用，不用重建镜像）
- **动画需求迭代要点**（用户偏好）：不要减淡只高亮；滚动条要"滚动游走"不要"进度填充"；程序启动即流动、就绪才停；重启点击瞬间起动画

## 工具调用经验（避免反复出错）

- **转义问题的第一原则：不要对抗转义，绕过它**。凡是命令里出现引号/反斜杠/反引号/多行/变量嵌套，立即改用以下已验证手法（按优先级）：
  1. **write 工具写脚本文件再执行**（本会话成功率最高）：复杂命令写成 `.ps1` 后 `powershell -NoProfile -ExecutionPolicy Bypass -File xxx.ps1`，或写成 `.cmd`/`.mjs`/`.js` 直接运行；假 npx、假 dsh、测试脚本、镜像代理全部走此方案
  2. **应用内调 PowerShell 一律 `-EncodedCommand`**（dsh.rs 的标准做法）：脚本 UTF-16LE → Base64 后传入，任何特殊字符（引号、反斜杠、换行、中文）都免转义，这是彻底方案
  3. **写简单文件用 here-string**：PowerShell `@'...'@`（单引号形式，不展开变量）配合 `Set-Content`，避免 Set-Content 的引号嵌套
  4. **TS 内联写 Rust/PowerShell 源码**：反斜杠一律双写 `\\`（见下条"JS 吃掉反斜杠"），写完必须 grep 验证；更复杂的含正则内容直接拆成独立文件维护
  5. **避免反引号**：PowerShell 换行 `\`n` 在 TS 模板字符串里要写成 `\\\`n` 极易错——用 here-string 或脚本文件替代
  6. **Start-Process 不要内联复杂参数**：`-ArgumentList '-e','require(...)'` 拼接不可靠，把逻辑写进脚本文件再 `node 文件`/`cmd /c 文件` 启动
- **read-before-write/edit**：文件工具要求先 read 才能写/编辑；文件被外部删除后 write 会报 `file no longer exists`——先确认存在（PowerShell 建目录/写文件亦可）
- **write 不自动建目录**：目标目录不存在时先 `New-Item -ItemType Directory -Force`
- **转义地狱**：run_code 的 TS 模板字符串里内联 PowerShell 极易转义出错（反引号、引号、\\）；复杂内容一律用 write 工具写成脚本文件再执行
- **JS 会吃掉无效转义的反斜杠（本项目的重大事故根源）**：在 TS 模板字符串中写 Rust/PowerShell 源码时，`\s`、`\d` 等无效转义序列会被 JS 解析为 `s`、`d`（反斜杠被丢弃）！本项目因此导致 stop 脚本正则 `"TCP\s+\S*:"` 静默变成 `"TCPs+S*:"`，杀 dsh 功能长期未真正生效。规则：**凡是要写入文件的反斜杠，TS 里一律双写 `\\`**（正则 `\s` 写 `\\s`，PowerShell 单引号 `'\\'` 写 `'\\\\'`，Rust 字符串 `\` 写 `\\\\`），写完必须用 grep 看实际行字节确认，不要依赖肉眼或 PowerShell 正则验证（验证命令自身的转义同样会骗人）
- **PowerShell 5.1 编码**：无 BOM 的 UTF-8 脚本中文字符串会按 GBK 乱码导致解析错误——脚本输出用英文，或保存为 UTF-8 BOM
- **沙箱查询限制**：`Get-NetTCPConnection` 常返回空（用 `netstat -ano`）；WMI `CommandLine` 可能为空；`Get-CimInstance` 可能被拒（用 netstat 代替）
- **后台任务**：长命令用 `run_in_background: true`，用 `job_output({wait: true})` 阻塞等待
- **git 状态**：`.cargo/`、根目录 exe 副本、test-bin 均不应入库；改动后检查 `git status` 是否干净
