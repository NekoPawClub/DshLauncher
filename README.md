# DshLauncher

![鲸鱼娘图标](icons/DeepSeekHarness-WhaleGirl.png)

DeepSeek Harness (dsh) 的 Windows 系统托盘启动器，使用 Rust 编写。

## 功能

- 常驻系统托盘，无主窗口；单实例运行 (重复启动自动退出)
- **守护保活 (watchdog)**：常驻守护线程周期探测 dsh 端口，dsh 未运行或意外退出时自动拉起 (已实测：dsh 进程崩溃后数秒内自动恢复)
- 启动时若 dsh 已在运行则直接复用 (不终结、不重启)
- **启动状态指示**：程序启动即让扫描灯流动 (2px 扫描仪灯管 + 两侧减淡灯光来回扫动，左右边界自然裁切)，只有 watchdog 探测到 dsh 可连接后才停止；重启点击瞬间立即起动画，并临时禁用重启菜单直到 dsh 重新就绪
- 托盘右键菜单 (自上而下)：
  - **打开**：用系统默认方式 (ShellExecute，无黑框) 打开 dsh 操作页面 (http://127.0.0.1:3080)；若 dsh 未运行则等待守护进程拉起，就绪后自动打开
  - **配置**：资源管理器打开 dsh 配置文件目录 (%USERPROFILE%/.dsh)；目录不存在时自动创建
  - **重启**：仅终结 dsh，由守护线程自动保活拉起 (npx -y @deepseek-ai/dsh web)，就绪后自动打开操作页面；重启期间重启菜单自动禁用，避免重复触发
  - **退出**：结束 Harness 并退出本程序
- 左键单击无功能；左键双击等同“打开”
- 保活细节：连续拉起失败 3 次后轮询节奏自动放慢 (2 秒 → 30 秒)，避免无效高频重试；退出时终结 dsh (Job 秒杀整个进程树，外部残留由纯 Win32 API 兜底清理)
- **启动超时以 dsh 输出为心跳**：连续 120 秒没有任何输出才放弃本轮等待；只要 dsh 还在持续输出就不判超时，托盘 tooltip 会定期显示等待时长与输出状态
- 外部残留兜底清理会先核对进程名 (node/cmd/dsh/npx)，不会因为端口配置错误而误杀无关服务

## 构建

- 前置要求：Rust 工具链 (MSVC target)、Windows SDK (提供 rc.exe，或设置环境变量 `RC_EXE` 指向 rc.exe)
- 构建命令：

  ```
  cargo build --release
  ```

- 产物：`target/release/DshLauncher.exe` (已内嵌图标，可直接使用或复制到桌面)。

## 版本与发布

- exe 版本号格式 `YY.MM.DD.NN` (年.月.日.当日第几次发布，各段固定两位补零，如 26.08.05.01)：CI 发布时经环境变量 `DSHLAUNCHER_VERSION` 传入 build.rs 嵌入；本地构建在 build.rs 重跑时取当天日期 + 0——源码变化会重跑并取新日期，只跨天而源码未变时重复构建保持旧版本
- 推送 main 且改动涉及编译文件时，CI 用 `git diff -b` 比较上一发布以来的源码：只有 `Cargo.toml` / `Cargo.lock` / `build.rs` / `src/` / `icons/` 出现非缩进变化才发布；源码无实质变化 (含仅缩进变化) 则跳过，发布时按北京时间当日计数生成 `vYY.MM.DD.NN` tag 与 Release (正文为上一发布以来的提交列表)
- 纯文档改动 (README/AGENTS 等) 不触发 CI

## 更新检测

- 启动即检查一次，之后每 1 小时自动复查；在线版本写入 `launcher.log`：进程启动写一次，跨凌晨 4 点日志日写一次，同一日志日仅远端出现新版本时增写；发现新版本由 Rust 直接通过 WinRT 发送 Windows 系统通知 (不再启动 PowerShell)
- 检测源：GitHub Releases API 直连为主，失败自动切换 gh-proxy 镜像；可用环境变量 `DSHLAUNCHER_UPDATE_MIRROR` 自定义镜像前缀，或用 `DSHLAUNCHER_UPDATE_DISABLE=1` 完全禁用检测
- 同一版本在最近 3 天日志记录内只提示一次 (去重依据为 `launcher.log` 中最近 3 天内最近一次“更新检测成功”记录的在线版本；记录过期清理后允许再次提示)，出现更新的版本后再提示；点击通知直接打开下载页面
- 通知在通知中心显示为 DshLauncher (发送前自动注册自有 AUMID，附带程序图标；发送失败会记录错误并在下一轮检查时重试)

## 端口

默认使用 3080；如需覆盖，请在启动 DshLauncher 前设置环境变量 `DSHLAUNCHER_PORT`。
设置后 DshLauncher 会用 `--port` 参数启动 dsh，探测、拉起与页面地址全部使用该端口。

## 说明

- 托盘图标与可执行文件图标均来自 `icons/DeepSeekHarness-WhaleGirl.ico`
- 图标来源：[deepseek-whale-girl-icon](https://github.com/fornarwhal/deepseek-whale-girl-icon.git)
- 单实例：重复启动会自动退出
- 运行日志：启动器事件与 dsh 输出统一写入 `%USERPROFILE%/.dsh/launcher.log` (单文件；启动器日志为 `[INFO]/[WARN]/[FAIL]`，dsh 的 stdout/stderr 按换行/缓冲块加时间标签与 `[DSH]` 标记；按每行时间标签保留最近 3 天，凌晨 4 点日界；更新检测与通知记录同样按普通日志清理；测试实例自动带后缀)；写入/查询/清理由后台日志管理线程串行执行，主线程只投递消息，不会被日志清理阻塞；可用环境变量 `DSHLAUNCHER_LOG_DIR` 覆盖日志目录
