# DshLauncher

![鲸鱼娘图标](icons/DeepSeekHarness-WhaleGirl.png)

DeepSeek Harness（dsh）的 Windows 系统托盘启动器，使用 Rust 编写。

## 功能

- 常驻系统托盘，无主窗口；单实例运行（重复启动自动退出）
- **守护保活（watchdog）**：常驻守护线程周期探测 dsh 端口，dsh 未运行或意外退出时自动拉起（已实测：dsh 进程崩溃后数秒内自动恢复）
- 启动时若 dsh 已在运行则直接复用（不终结、不重启）
- **启动状态指示**：程序启动即让扫描灯流动（扫描仪灯管效果：不透明全白灯管 + 两侧减淡灯光来回扫动，灯光溢出边缘自然裁剪），watchdog 探测到 dsh 就绪后自动停止；重启点击瞬间立即起动画
- 托盘右键菜单（自上而下）：
  - **打开**：用系统默认方式（ShellExecute，无黑框）打开 dsh 操作页面（http://127.0.0.1:3080）；若 dsh 未运行则等待守护进程拉起，就绪后自动打开
  - **配置**：资源管理器打开 dsh 配置文件目录（%USERPROFILE%/.dsh）；目录不存在时自动创建
  - **重启**：仅终结 dsh，由守护线程自动保活拉起（npx -y @deepseek-ai/dsh web），就绪后自动打开操作页面
  - **退出**：结束 Harness 并退出本程序
- 左键单击无功能；左键双击等同“打开”
- 保活细节：连续拉起失败 3 次后轮询节奏自动放慢（2 秒 → 30 秒），避免无效高频重试；退出时终结 dsh（Job 秒杀整个进程树，外部残留兜底脚本清理）

## 构建

- 前置要求：Rust 工具链（MSVC target）、Windows SDK（提供 rc.exe，或设置环境变量 `RC_EXE` 指向 rc.exe）
- 构建命令：

  ```
  cargo build --release
  ```

- 产物：`target/release/DshLauncher.exe`（已内嵌图标，可直接使用或复制到桌面）。

## 端口

默认使用 3080；如需覆盖，请在启动 DshLauncher 前设置环境变量 `DSHLAUNCHER_PORT`。
设置后 DshLauncher 会用 `--port` 参数启动 dsh，探测、拉起与页面地址全部使用该端口。

## 说明

- 托盘图标与可执行文件图标均来自 `icons/DeepSeekHarness-WhaleGirl.ico`
- 图标来源：[deepseek-whale-girl-icon](https://github.com/fornarwhal/deepseek-whale-girl-icon.git)
- 单实例：重复启动会自动退出
- 运行日志：写入 `%USERPROFILE%/.dsh/launcher.log`（单文件，按每行时间标签保留最近 3 天，凌晨 4 点日界；测试实例自动带后缀）；可用环境变量 `DSHLAUNCHER_LOG_DIR` 覆盖日志目录
