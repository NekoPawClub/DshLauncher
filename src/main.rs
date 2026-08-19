//! DshLauncher —— DeepSeek Harness 守护启动器
//!
//! 常驻系统托盘、无主窗口。托盘右键菜单：打开 / 配置 / 重启 / 退出；
//! 左键单击无功能，左键双击等同"打开"。
//! Harness 的启动命令：npx -y @deepseek-ai/dsh web (端口覆盖时附加 --port)。
//!
//! 守护 (watchdog) 设计：
//! - 常驻守护线程周期探测 dsh 端口：dsh 未运行即自动保活拉起
//! - 动画策略：程序启动即让扫描灯流动，watchdog 探测到 dsh 就绪才停止；
//!   重启点击瞬间立即起动画，dsh 重启结束 (端口就绪) 后停止
//! - 启动器启动时若 dsh 已在运行 (端口连通) 则直接复用，不终结不重启

#![windows_subsystem = "windows"]

mod dsh;
mod log;
mod toast;
mod update;

use std::error::Error;
use std::io;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder, TrayIconEvent};
use windows_sys::Win32::Foundation::HANDLE;
use winit::application::ApplicationHandler;
use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};

/// 事件循环的自定义用户事件
enum UserEvent {
    Menu(MenuEvent),
    Tray(TrayIconEvent),
    /// 动画换帧 (动画线程按 anim_running 驱动)
    AnimationTick,
    /// 动画开关由 true 变为 false：恢复默认图标 (若期间又被置 true 则忽略)
    AnimationStop,
    /// 启动流程结束：就绪且有待办时打开页面
    AnimationDone {
        ready: bool,
    },
    /// tooltip 状态更新：dsh 运行状态显示
    TooltipUpdate {
        ready: bool,
    },
    /// 后台端口探测完成：探测在后台线程执行，主线程不等待探测结果
    OpenProbeDone {
        ready: bool,
    },
    /// 启动等待进度：只要 dsh 持续输出，就不判定为超时，并通过 tooltip 告知用户
    StartupProgress {
        elapsed_secs: u64,
        output_active: bool,
    },
}

/// 动画唯一状态开关：只修改 AtomicBool，不直接向主线程发帧/停止事件。
/// 重复置 true 只刷新标志，不改变当前动画运行状态；置 false 后由动画线程
/// 观察到下降沿时统一发送 AnimationStop。
fn set_anim(anim_running: &AtomicBool, running: bool) {
    anim_running.store(running, Ordering::SeqCst);
}

/// RAII 守卫：离开作用域 (含 panic 与提前 return) 时释放 starting 标志，
/// 防止 flow 线程异常退出后 watchdog 永久停摆。
struct StartingGuard(Arc<AtomicBool>);

impl Drop for StartingGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// 托盘图标组：默认图标 + 启动动画帧 (扫描仪灯管)
struct TrayIcons {
    default: Icon,
    frames: Vec<Icon>,
}

/// 启动等待的最大无输出时长：只要 dsh 还在持续输出信息，就不按固定时长判超时。
const STARTUP_IDLE_TIMEOUT: Duration = Duration::from_secs(120);
/// 启动进度 tooltip 的更新间隔。
const STARTUP_PROGRESS_INTERVAL: Duration = Duration::from_secs(5);
/// 多久内有输出视为“仍在持续输出”。
const OUTPUT_ACTIVE_WINDOW: Duration = Duration::from_secs(5);

/// 启动流程线程：端口被占才清理 (Job 设计保证常态无残留) → 启动 → 等待就绪。
/// 等待以 dsh 输出活动为心跳：连续 120 秒无输出才结束，持续有输出不超时；
/// 期间通过 StartupProgress 事件更新 tooltip，让用户知道等待仍在进行。
/// 动画由独立动画线程按 anim_running 开关驱动，本线程不操作动画。
fn spawn_startup_flow(
    proxy: EventLoopProxy<UserEvent>,
    starting: Arc<AtomicBool>,
    quitting: Arc<AtomicBool>,
) -> io::Result<()> {
    thread::Builder::new()
        .name("dsh-launcher-startup".to_string())
        .spawn(move || {
            // RAII 守卫：任何退出路径 (含 panic) 都会释放 starting。
            // 正常结束时先显式释放，再发送 AnimationDone：
            // 主线程处理事件时可读到实时 starting 状态，不会把旧流程误判为新流程。
            let _guard = StartingGuard(starting.clone());
            let ready = run_startup_flow(&proxy, &quitting);
            drop(_guard);
            let _ = proxy.send_event(UserEvent::AnimationDone { ready });
        })?;
    Ok(())
}

/// 启动流程主体：端口被占才清理 → 启动 → 等待就绪。返回最终是否就绪。
/// starting 标志由 spawn_startup_flow 的 RAII 守卫管理。
fn run_startup_flow(proxy: &EventLoopProxy<UserEvent>, quitting: &AtomicBool) -> bool {
    if quitting.load(Ordering::SeqCst) {
        return false;
    }
    log::info("启动流程开始");
    // tooltip 立即显示"启动中" (watchdog 的下一次状态变化可能滞后 2 秒)
    let _ = proxy.send_event(UserEvent::TooltipUpdate { ready: false });

    // 仅当端口被残留进程占用时清理 (罕见：非 Job 管理的外部进程；
    // Job + KILL_ON_JOB_CLOSE 保证我们启动的 dsh 崩溃即释放端口，常态零清理)
    if dsh::port_occupied() {
        log::info("端口被外部进程占用，先执行清理");
        dsh::stop_harness();
        // 等待端口释放 (最多 5 秒)
        let wf_deadline = Instant::now() + Duration::from_secs(5);
        while dsh::port_occupied() && Instant::now() < wf_deadline {
            thread::sleep(Duration::from_millis(200));
        }
    }
    if quitting.load(Ordering::SeqCst) {
        return false;
    }

    let mut ready = dsh::port_ready();
    let mut waited_for_port = false;
    let mut idle_timeout = false;
    if !ready {
        // 当前启动流程的输出活动追踪器：pump_dsh_output 每读到数据就 touch。
        let activity = Arc::new(dsh::OutputActivity::new());
        match dsh::start_harness(quitting, activity.clone()) {
            Ok(()) => {
                waited_for_port = true;
                let started = Instant::now();
                let mut last_progress = started;
                while !ready && !quitting.load(Ordering::SeqCst) {
                    ready = dsh::port_ready();
                    if ready {
                        break;
                    }
                    let idle = activity.elapsed();
                    if idle >= STARTUP_IDLE_TIMEOUT {
                        // 连续 120 秒没有新输出，视为当前拉起卡死。
                        idle_timeout = true;
                        log::warn("等待 dsh 端口就绪：已 120 秒无新输出，结束当前等待");
                        break;
                    }
                    if last_progress.elapsed() >= STARTUP_PROGRESS_INTERVAL {
                        let _ = proxy.send_event(UserEvent::StartupProgress {
                            elapsed_secs: started.elapsed().as_secs(),
                            output_active: activity.has_received_output()
                                && idle <= OUTPUT_ACTIVE_WINDOW,
                        });
                        last_progress = Instant::now();
                    }
                    thread::sleep(Duration::from_millis(500));
                }
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::Interrupted
                    && quitting.load(Ordering::SeqCst) =>
            {
                log::info("启动流程收到退出请求，取消启动 dsh");
            }
            Err(e) => {
                log::error(&format!("启动 dsh 失败：{e}"));
            }
        }
    }
    if !ready && !quitting.load(Ordering::SeqCst) {
        if idle_timeout {
            // 清理当前卡住的启动树，watchdog 重试前不叠加 npx/node。
            log::warn("等待 dsh 端口就绪结束：清理当前启动树，watchdog 将重试");
            dsh::stop_harness();
        } else if waited_for_port {
            log::warn("等待 dsh 端口就绪结束，watchdog 将重试");
        } else {
            log::warn("dsh 启动失败，watchdog 将重试");
        }
    }

    log::info(&format!("启动流程结束 (就绪 = {ready})"));
    ready
}

/// 应用主体：持有托盘图标与各菜单项 id
struct App {
    tray: TrayIcon,
    default_icon: Icon,
    anim_frames: Vec<Icon>,
    anim_idx: usize,
    /// 动画是否在播放 (扫描灯流动中)
    anim_running: Arc<AtomicBool>,
    /// 是否有 dsh 启动流程进行中 (防重入)
    starting: Arc<AtomicBool>,
    /// 启动期间用户请求"打开页面"的待办标记
    pending_open: Arc<AtomicBool>,
    /// 待办是否来自"重启"菜单 (重启失败时清除待办，避免反复自动打开)
    pending_from_restart: Arc<AtomicBool>,
    /// 退出请求标志：守护线程与启动流程据此停止
    quitting: Arc<AtomicBool>,
    /// 单实例互斥体句柄 (退出时提前释放，避免新实例被单例阻止启动)
    mutex_handle: Option<HANDLE>,
    proxy: EventLoopProxy<UserEvent>,
    open_id: MenuId,
    config_id: MenuId,
    restart_id: MenuId,
    quit_id: MenuId,
    /// 重启菜单项句柄：点击后立即禁用，dsh 重新就绪后恢复，防止并发重复重启。
    restart_item: MenuItem,
}

impl App {
    /// 重启菜单的启用条件：dsh 可连接、无进行中的启动流程、
    /// 无重启待办且未在退出。其它状态一律禁用，避免在启动/停止过程中重复触发。
    fn restart_item_available(&self) -> bool {
        !self.quitting.load(Ordering::SeqCst)
            && !self.starting.load(Ordering::SeqCst)
            && !self.pending_from_restart.load(Ordering::SeqCst)
    }

    /// 按 dsh 就绪状态同步重启菜单项：就绪且满足启用条件才可点击。
    fn sync_restart_item(&self, ready: bool) {
        self.restart_item
            .set_enabled(ready && self.restart_item_available());
    }

    /// 动画线程：只观察 anim_running 开关。
    /// true 期间每 150ms 发一帧 AnimationTick；true→false 下降沿才发一次
    /// AnimationStop。重复置 true 不产生额外状态变化。
    fn spawn_animator(&self) {
        let anim_running = self.anim_running.clone();
        let quitting = self.quitting.clone();
        let proxy = self.proxy.clone();
        thread::spawn(move || {
            let mut was_running = anim_running.load(Ordering::SeqCst);
            loop {
                if quitting.load(Ordering::SeqCst) {
                    break;
                }
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let running = anim_running.load(Ordering::SeqCst);
                    if !running && was_running {
                        let _ = proxy.send_event(UserEvent::AnimationStop);
                    }
                    was_running = running;
                    if running {
                        let _ = proxy.send_event(UserEvent::AnimationTick);
                    }
                    thread::sleep(Duration::from_millis(150));
                }));
                if result.is_err() {
                    // 单次循环异常时重读开关，避免下降沿状态丢失。
                    log::error("动画线程单次循环异常，150ms 后继续");
                    was_running = anim_running.load(Ordering::SeqCst);
                    thread::sleep(Duration::from_millis(150));
                }
            }
        });
    }

    /// 守护线程：周期检查 dsh 状态；状态变化时驱动动画启停
    /// (未就绪扫描灯流动、就绪停止)，dsh 未运行则自动保活拉起。
    /// 连续拉起失败 3 次后放慢轮询节奏，避免无效高频重试。
    fn spawn_watchdog(&self) {
        let proxy = self.proxy.clone();
        let starting = self.starting.clone();
        let quitting = self.quitting.clone();
        let anim_running = self.anim_running.clone();
        thread::spawn(move || {
            let mut fail_count: u32 = 0;
            let mut was_ready: Option<bool> = None;
            loop {
                if quitting.load(Ordering::SeqCst) {
                    break;
                }
                let result = catch_unwind(AssertUnwindSafe(|| {
                    let ready = dsh::port_ready();
                    // 状态变化：驱动动画 (就绪停、未就绪跑)
                    if was_ready != Some(ready) {
                        // 动画只由 watchdog 的“dsh 是否可连接”状态变化驱动：
                        // 未就绪置 true，可连接置 false。
                        set_anim(&anim_running, !ready);
                        was_ready = Some(ready);
                        // tooltip 同步运行状态 (flow 进行中时显示"启动中")
                        let _ = proxy.send_event(UserEvent::TooltipUpdate { ready });
                        if ready {
                            log::info(&format!("dsh 就绪 (端口 {} 可连接)", dsh::web_port()));
                        } else {
                            log::info("dsh 未就绪");
                        }
                    }
                    if ready {
                        fail_count = 0;
                    } else if !starting.swap(true, Ordering::SeqCst) {
                        // dsh 未运行且无启动流程：保活拉起 (端口被占时 flow 内自动清理)
                        log::info("dsh 未运行，触发保活拉起");
                        match spawn_startup_flow(proxy.clone(), starting.clone(), quitting.clone())
                        {
                            Ok(()) => fail_count += 1,
                            Err(e) => {
                                starting.store(false, Ordering::SeqCst);
                                log::error(&format!("启动流程线程创建失败：{e}"));
                                fail_count += 1;
                            }
                        }
                    }
                    // 连续失败 3 次以上放慢到 30 秒一查
                    let interval = if fail_count >= 3 {
                        if fail_count == 3 {
                            log::warn("连续拉起失败 3 次，轮询节奏放慢到 30 秒");
                        }
                        Duration::from_secs(30)
                    } else {
                        Duration::from_secs(2)
                    };
                    thread::sleep(interval);
                }));
                if result.is_err() {
                    // 单次检查异常时重读状态，下一轮强制同步动画与 tooltip。
                    log::error("watchdog 单次检查异常，2 秒后继续");
                    was_ready = None;
                    set_anim(&anim_running, true);
                    thread::sleep(Duration::from_secs(2));
                }
            }
        });
    }

    /// 更新托盘 tooltip：反映 dsh 运行状态
    fn update_tooltip(&self, ready: bool, starting: bool) {
        let text = if ready {
            format!("DshLauncher — dsh 运行中 (端口 {})", dsh::web_port())
        } else if starting {
            "DshLauncher — dsh 启动中…".to_string()
        } else {
            "DshLauncher — dsh 未运行".to_string()
        };
        let _ = self.tray.set_tooltip(Some(&text));
    }

    /// 启动等待进度 tooltip：明确告诉用户等待仍在进行，以及 dsh 是否还有输出。
    fn update_startup_progress(&self, elapsed_secs: u64, output_active: bool) {
        let state = if output_active {
            "进程持续输出中"
        } else {
            "暂未收到新输出"
        };
        let text = format!("DshLauncher — dsh 启动中… (已等待 {elapsed_secs}s，{state})");
        let _ = self.tray.set_tooltip(Some(&text));
    }

    /// "打开"处理：先在后台探测 dsh 端口 (避免 500ms 探测阻塞主线程)，
    /// 同时立即登记待办并让扫描灯流动；探测到已就绪则直接打开并停止动画。
    fn handle_open(&self) {
        log::info("用户请求打开 dsh，后台探测端口");
        self.pending_open.store(true, Ordering::SeqCst);
        self.pending_from_restart.store(false, Ordering::SeqCst);
        set_anim(&self.anim_running, true);
        // 与动画同款即时反馈：tooltip 立即显示"启动中"
        self.update_tooltip(false, true);

        let proxy = self.proxy.clone();
        thread::spawn(move || {
            let ready = dsh::port_ready();
            let _ = proxy.send_event(UserEvent::OpenProbeDone { ready });
        });
    }

    /// 后台打开 dsh 页面：ShellExecuteW 不占用主线程，失败写入日志。
    fn open_page_async() {
        thread::spawn(|| {
            if let Err(e) = dsh::open_page() {
                log::error(&format!("打开 dsh 页面失败：{e}"));
            }
        });
    }

    /// 更新检测线程：启动即首查一次，之后每 1 小时复查；
    /// 发现更新由检测线程直接通过 WinRT 发 Windows 通知 (3 天日志窗口内去重)
    fn spawn_checker(&self) {
        update::spawn_checker(self.quitting.clone());
    }

    /// 退出处理：置退出标志 → 隐藏托盘图标 (即时反馈) → 提前释放单例互斥体
    /// (新实例可立即启动) → 退出事件循环。
    /// dsh 清理在 run_app 返回后执行 (main 尾部)，避免主线程阻塞；
    /// KILL_ON_JOB_CLOSE 兜底崩溃场景。
    fn handle_quit(&mut self, event_loop: &ActiveEventLoop) {
        self.quitting.store(true, Ordering::SeqCst);
        // 立即隐藏托盘图标 + 关闭动画开关 (即时反馈)
        let _ = self.tray.set_visible(false);
        self.restart_item.set_enabled(false);
        set_anim(&self.anim_running, false);
        // 提前释放单例互斥体：此时进程尚未退出，但新实例可以立即启动
        // (新实例的 watchdog 会自动接管 dsh 状态，旧进程仅剩清理收尾)
        if let Some(handle) = self.mutex_handle.take() {
            dsh::release_single_instance(handle);
        }
        event_loop.exit();
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        // 无窗口应用：无需额外初始化
    }

    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        _event: winit::event::WindowEvent,
    ) {
        // 本程序不创建任何窗口，此回调不会触发
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UserEvent) {
        match event {
            UserEvent::Menu(ev) => {
                if ev.id() == &self.open_id {
                    // 打开：检查 dsh 运行状态，未运行先启动
                    self.handle_open();
                } else if ev.id() == &self.config_id {
                    // 配置：资源管理器打开 dsh 配置目录 ~/.dsh。
                    // create_dir_all/ShellExecuteW 放到后台，主线程只负责菜单响应。
                    log::info("用户请求打开配置目录");
                    thread::spawn(|| {
                        if let Err(e) = dsh::open_config_dir() {
                            log::error(&format!("打开配置目录失败：{e}"));
                        }
                    });
                } else if ev.id() == &self.restart_id {
                    // 重启菜单只在 dsh 就绪且无启动流程时可点击；这里再做一次防御性检查。
                    if !self.restart_item.is_enabled() {
                        return;
                    }
                    // 重启：立即禁用重启菜单，防止并发重复重启；动画置 true 只改开关。
                    // 终结 dsh 必须在后台线程执行，避免阻塞主线程导致动画延迟。
                    log::info("用户请求重启 dsh");
                    self.restart_item.set_enabled(false);
                    set_anim(&self.anim_running, true);
                    self.pending_open.store(true, Ordering::SeqCst);
                    self.pending_from_restart.store(true, Ordering::SeqCst);
                    // 与动画同款即时反馈：tooltip 立即显示"启动中"
                    self.update_tooltip(false, true);
                    thread::spawn(dsh::stop_harness);
                } else if ev.id() == &self.quit_id {
                    // 退出：终结 dsh 并停止守护后退出本程序
                    self.handle_quit(event_loop);
                }
            }
            UserEvent::Tray(ev) => {
                // 左键双击等同"打开"；单击等其他事件一律忽略
                if matches!(ev, TrayIconEvent::DoubleClick { .. }) {
                    self.handle_open();
                }
            }
            UserEvent::AnimationTick => {
                // 动画播放中才换帧 (停止后到达的遗留帧忽略)
                if self.anim_running.load(Ordering::SeqCst) {
                    self.anim_idx = (self.anim_idx + 1) % self.anim_frames.len();
                    let _ = self
                        .tray
                        .set_icon(Some(self.anim_frames[self.anim_idx].clone()));
                }
            }
            UserEvent::AnimationStop => {
                // 下降沿恢复默认图标；若处理前开关又被置 true，则忽略这次遗留停止。
                if !self.anim_running.load(Ordering::SeqCst) {
                    self.anim_idx = 0;
                    let _ = self.tray.set_icon(Some(self.default_icon.clone()));
                }
            }
            UserEvent::AnimationDone { ready } => {
                // 不在这里手动停动画：动画启停只由 watchdog 的“dsh 可连接”状态决定。
                // flow 在发送本事件前已释放 starting；按实时状态刷新 tooltip。
                self.update_tooltip(ready, self.starting.load(Ordering::SeqCst));
                if ready {
                    // dsh 已拉起：清除重启待办、恢复重启菜单，并消费待办打开页面。
                    self.pending_from_restart.store(false, Ordering::SeqCst);
                    self.sync_restart_item(true);
                    if self.pending_open.swap(false, Ordering::SeqCst) {
                        Self::open_page_async();
                    }
                } else {
                    self.restart_item.set_enabled(false);
                    if self.pending_from_restart.swap(false, Ordering::SeqCst) {
                        // 重启失败：清除待办 (避免 dsh 恢复后反复自动打开无意义页面)，保留日志。
                        if self.pending_open.swap(false, Ordering::SeqCst) {
                            log::warn("重启失败：已清除打开待办 (dsh 未就绪，watchdog 将保活重试)");
                        }
                    }
                }
                // "打开"来源的待办在失败时保留：等下次就绪再消费，此处不处理。
            }
            UserEvent::TooltipUpdate { ready } => {
                self.sync_restart_item(ready);
                // 实时读 starting：事件到达时若 flow 已结束则显示最终状态，
                // 避免竞态 (watchdog 事件与 AnimationDone 乱序) 导致 tooltip 卡"启动中"
                self.update_tooltip(ready, self.starting.load(Ordering::SeqCst));
            }
            UserEvent::OpenProbeDone { ready } => {
                if ready {
                    // 仅当没有重启流程时，这个“可连接”探测结果才允许关闭动画/打开页面；
                    // 重启期间的旧探测结果不能覆盖重启动画，也不能消费重启待办。
                    if !self.pending_from_restart.load(Ordering::SeqCst) {
                        set_anim(&self.anim_running, false);
                        self.update_tooltip(true, false);
                        self.sync_restart_item(true);
                        if self.pending_open.swap(false, Ordering::SeqCst) {
                            Self::open_page_async();
                        }
                    }
                } else {
                    log::info("dsh 未运行：登记打开待办，由 watchdog 拉起后就绪自动打开");
                }
            }
            UserEvent::StartupProgress {
                elapsed_secs,
                output_active,
            } => {
                // 旧 flow 的迟到进度在 starting 已释放/被新 flow 占用时不显示。
                if self.starting.load(Ordering::SeqCst) {
                    self.update_startup_progress(elapsed_secs, output_active);
                }
            }
        }
    }
}

/// 加载托盘图标：默认图标 (ICO 按 PNG 裁剪比例取中心 → 缩放 32x32)
/// + 启动动画帧 (扫描仪灯管来回扫动)
fn load_tray_icons() -> Result<TrayIcons, Box<dyn Error>> {
    use image::Rgba;

    let ico_bytes: &[u8] = include_bytes!("../icons/DeepSeekHarness-WhaleGirl.ico");
    let image = image::load_from_memory_with_format(ico_bytes, image::ImageFormat::Ico)?.to_rgba8();

    // 裁剪规则：以 PNG (984x984) 中心裁剪 760x760 的比例为基准，
    // 等比应用到图标源 (ICO 256x256 → 中心 198x198)，保留中心内容
    const CROP_RATIO: f64 = 760.0 / 984.0; // ≈ 0.7724
    let (w, h) = image.dimensions();
    let side = w.min(h) as f64;
    let crop = (side * CROP_RATIO).round() as u32;
    let ox = (w - crop) / 2;
    let oy = (h - crop) / 2;
    let cropped = image::imageops::crop_imm(&image, ox, oy, crop, crop).to_image();
    let base = image::imageops::resize(&cropped, 32, 32, image::imageops::FilterType::Lanczos3);
    let default = Icon::from_rgba(base.to_vec(), 32, 32)?;

    const FRAME_COUNT: usize = 16;
    let mut frames = Vec::with_capacity(FRAME_COUNT);
    for i in 0..FRAME_COUNT {
        let t = i as f32 / FRAME_COUNT as f32; // 0..1 一循环

        // 图标大小不变：直接铺满画布
        let mut frame = base.clone();

        // 扫描仪灯管效果：2px 不透明全白灯管 + 单侧 6px 线性衰减半透明灯光。
        // 灯管中心用连续坐标在 1.0~31.0 之间三角波扫动；逐列用像素中心
        // (x+0.5) 计算覆盖，左右边界处灯管自然被画布裁切，保持 2px 宽度。
        const LAMP_HALF: f32 = 1.0; // 灯管半宽 (总宽 2px)
        const LIGHT_W: f32 = 6.0; // 单侧灯光延伸宽度 (线性衰减)
        const LIGHT_MAX: f32 = 180.0; // 灯光最大强度 (紧邻灯管的理论值)
        let travel = 32.0 - 2.0 * LAMP_HALF; // 灯管中心移动范围：1.0 ~ 31.0
        let tri = (2.0 * t).fract();
        let wave = 1.0 - (2.0 * tri - 1.0).abs();
        let lamp_center = LAMP_HALF + wave * travel;

        // 逐列计算灯光强度并叠加 (alpha 混合，透明区域也能被灯光照亮)
        for x in 0..32u32 {
            let pixel_center = x as f32 + 0.5;
            let dist = (pixel_center - lamp_center).abs();
            let alpha = if dist <= LAMP_HALF {
                255.0 // 灯管：不透明全白
            } else {
                let fall = dist - LAMP_HALF;
                if fall < LIGHT_W {
                    LIGHT_MAX * (LIGHT_W - fall) / LIGHT_W
                } else {
                    0.0
                }
            };
            let alpha = alpha.round().clamp(0.0, 255.0) as u8;
            if alpha > 0 {
                let line = image::RgbaImage::from_pixel(1, 32, Rgba([255, 255, 255, alpha]));
                image::imageops::overlay(&mut frame, &line, x as i64, 0);
            }
        }

        frames.push(Icon::from_rgba(frame.into_raw(), 32, 32)?);
    }

    Ok(TrayIcons { default, frames })
}

fn main() {
    // windows_subsystem 下没有控制台：panic 也写入日志，并保留默认 hook 行为。
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let message = format!("线程 panic：{info}");
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            log::error(&message);
            log::flush();
        }));
        default_hook(info);
    }));

    if let Err(e) = run() {
        // 启动失败写日志，避免错误完全不可见。
        log::error(&format!("DshLauncher 运行失败：{e}"));
        log::flush();
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    // 单实例保护：已有 DshLauncher 在运行时，本实例直接退出；
    // 句柄存入 App，退出时提前释放以便新实例立即启动
    let mutex_handle = match dsh::single_instance_guard()? {
        Some(handle) => Some(handle),
        None => return Ok(()),
    };

    // 托盘右键菜单：打开 / 配置 / 重启 / 退出 (自上而下)
    let menu = Menu::new();
    let open_item = MenuItem::new("打开", true, None);
    let config_item = MenuItem::new("配置", true, None);
    let restart_item = MenuItem::new("重启", false, None);
    let quit_item = MenuItem::new("退出", true, None);
    menu.append_items(&[&open_item, &config_item, &restart_item, &quit_item])?;

    // 记录菜单项 id，用于事件分发
    let open_id = open_item.id().clone();
    let config_id = config_item.id().clone();
    let restart_id = restart_item.id().clone();
    let quit_id = quit_item.id().clone();

    // 事件循环 (不创建任何窗口，仅承载托盘事件)
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    // 把菜单事件与托盘图标事件转发到事件循环 (两个闭包各持一份代理)
    let menu_proxy = proxy.clone();
    let tray_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event));
    }));
    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = tray_proxy.send_event(UserEvent::Tray(event));
    }));

    // 加载图标并创建托盘：左键单击不弹出菜单 (无功能)，右键显示菜单
    let icons = load_tray_icons()?;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip("DshLauncher — 启动中…")
        .with_icon(icons.default.clone())
        .with_menu_on_left_click(false)
        .build()?;

    let mut app = App {
        tray,
        default_icon: icons.default,
        anim_frames: icons.frames,
        anim_idx: 0,
        anim_running: Arc::new(AtomicBool::new(false)),
        starting: Arc::new(AtomicBool::new(false)),
        pending_open: Arc::new(AtomicBool::new(false)),
        pending_from_restart: Arc::new(AtomicBool::new(false)),
        quitting: Arc::new(AtomicBool::new(false)),
        mutex_handle,
        proxy,
        open_id,
        config_id,
        restart_id,
        quit_id,
        restart_item,
    };

    // 程序启动即让扫描灯流动；watchdog 探测到 dsh 可连接后才关闭开关
    set_anim(&app.anim_running, true);
    // 动画线程 + 守护线程
    app.spawn_animator();
    app.spawn_watchdog();
    app.spawn_checker();

    event_loop.run_app(&mut app)?;

    // 仅主动退出 (handle_quit 已置 quitting) 时同步清理 dsh：
    // Job 秒杀 + 外部残留兜底；KILL_ON_JOB_CLOSE 兜底崩溃与异常退出场景
    if app.quitting.load(Ordering::SeqCst) {
        dsh::stop_harness();
    }

    // 日志已异步化：正常退出前等待日志管理线程处理完队列中的日志。
    log::flush();

    Ok(())
}
