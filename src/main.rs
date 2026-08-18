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
mod update;

use std::error::Error;
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
    /// 动画停止：恢复默认图标
    AnimationStop,
    /// 启动流程结束：就绪且有待办时打开页面
    AnimationDone {
        ready: bool,
    },
    /// tooltip 状态更新：dsh 运行状态显示
    TooltipUpdate {
        ready: bool,
    },
}

/// 切换动画状态：running=true 启动 (扫描灯流动)，false 停止 (恢复默认图标)。
/// 供 watchdog / 启动流程 / 菜单等任意线程调用。
fn set_anim(anim_running: &AtomicBool, running: bool, proxy: &EventLoopProxy<UserEvent>) {
    if running {
        anim_running.store(true, Ordering::SeqCst);
    } else if anim_running.swap(false, Ordering::SeqCst) {
        let _ = proxy.send_event(UserEvent::AnimationStop);
    }
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
struct Trayicons {
    default: Icon,
    frames: Vec<Icon>,
}

/// 启动流程线程：端口被占才清理 (Job 设计保证常态无残留) → 启动 → 等待就绪。
/// 动画由独立动画线程驱动 (探测到未运行即已流动)，本线程只负责流程；
/// 各阶段都会检查退出请求 (quitting)，退出时立即放弃。
fn spawn_startup_flow(
    proxy: EventLoopProxy<UserEvent>,
    starting: Arc<AtomicBool>,
    quitting: Arc<AtomicBool>,
    anim_running: Arc<AtomicBool>,
) {
    thread::spawn(move || {
        // RAII 守卫：任何退出路径 (含 panic) 都会释放 starting
        let _guard = StartingGuard(starting.clone());
        if quitting.load(Ordering::SeqCst) {
            return;
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
            return;
        }

        let mut ready = dsh::port_ready();
        if !ready && dsh::start_harness(&quitting).is_ok() {
            // 轮询端口就绪 (500ms 间隔，最长 120 秒)
            let deadline = Instant::now() + Duration::from_secs(120);
            while !ready && Instant::now() < deadline && !quitting.load(Ordering::SeqCst) {
                ready = dsh::port_ready();
                if ready {
                    break;
                }
                thread::sleep(Duration::from_millis(500));
            }
        }
        if !ready && !quitting.load(Ordering::SeqCst) {
            log::warn("等待 dsh 端口就绪超时 (120 秒)，watchdog 将重试");
        }

        // 就绪后停止动画 (若在播放)
        if ready {
            set_anim(&anim_running, false, &proxy);
        }
        log::info(&format!("启动流程结束 (就绪 = {ready})"));
        let _ = proxy.send_event(UserEvent::AnimationDone { ready });
    });
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
}

impl App {
    /// 动画线程：动画播放期间每 150ms 发送一帧换帧事件 (不播放时仅轻量轮询)
    fn spawn_animator(&self) {
        let anim_running = self.anim_running.clone();
        let quitting = self.quitting.clone();
        let proxy = self.proxy.clone();
        thread::spawn(move || loop {
            if quitting.load(Ordering::SeqCst) {
                break;
            }
            if anim_running.load(Ordering::SeqCst) {
                let _ = proxy.send_event(UserEvent::AnimationTick);
            }
            thread::sleep(Duration::from_millis(150));
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
                let ready = dsh::port_ready();
                // 状态变化：驱动动画 (就绪停、未就绪跑)
                if was_ready != Some(ready) {
                    set_anim(&anim_running, !ready, &proxy);
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
                    spawn_startup_flow(
                        proxy.clone(),
                        starting.clone(),
                        quitting.clone(),
                        anim_running.clone(),
                    );
                    fail_count += 1;
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

    /// "打开"处理：dsh 已运行则直接打开；
    /// 未运行则登记待办并立即让扫描灯流动，由守护线程拉起，就绪后自动打开。
    fn handle_open(&self) {
        if dsh::port_ready() {
            let _ = dsh::open_page();
        } else {
            self.pending_open.store(true, Ordering::SeqCst);
            self.pending_from_restart.store(false, Ordering::SeqCst);
            set_anim(&self.anim_running, true, &self.proxy);
            // 与动画同款即时反馈：tooltip 立即显示"启动中"
            self.update_tooltip(false, true);
            log::info("dsh 未运行：登记打开待办，由 watchdog 拉起后就绪自动打开");
        }
    }

    /// 更新检测线程：启动即首查一次，之后每 1 小时复查；
    /// 发现更新由检测线程直接发 Windows 通知 (去重后每版本仅提示一次)
    fn spawn_checker(&self) {
        update::spawn_checker(self.quitting.clone());
    }

    /// 退出处理：置退出标志 → 隐藏托盘图标 (即时反馈) → 提前释放单例互斥体
    /// (新实例可立即启动) → 退出事件循环。
    /// dsh 清理在 run_app 返回后执行 (main 尾部)，避免主线程阻塞；
    /// KILL_ON_JOB_CLOSE 兜底崩溃场景。
    fn handle_quit(&mut self, event_loop: &ActiveEventLoop) {
        self.quitting.store(true, Ordering::SeqCst);
        // 立即隐藏托盘图标 + 停止动画 (即时反馈)
        let _ = self.tray.set_visible(false);
        set_anim(&self.anim_running, false, &self.proxy);
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
                    // 配置：资源管理器打开 dsh 配置目录 ~/.dsh
                    log::info("用户请求打开配置目录");
                    if let Err(e) = dsh::open_config_dir() {
                        log::error(&format!("打开配置目录失败：{e}"));
                    }
                } else if ev.id() == &self.restart_id {
                    // 重启：立即让扫描灯流动 (动画由主线程事件循环驱动，
                    // 终结 dsh 必须在后台线程执行，避免阻塞主线程导致动画延迟)
                    log::info("用户请求重启 dsh");
                    set_anim(&self.anim_running, true, &self.proxy);
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
                // 停止动画：恢复默认图标
                self.anim_idx = 0;
                let _ = self.tray.set_icon(Some(self.default_icon.clone()));
            }
            UserEvent::AnimationDone { ready } => {
                // 确保动画停止 (若 flow 已停则幂等)
                set_anim(&self.anim_running, false, &self.proxy);
                // flow 已结束 (starting 已释放)：tooltip 显示最终状态
                self.update_tooltip(ready, false);
                if ready {
                    // 成功：消费待办 (无论来源) 并打开页面
                    self.pending_from_restart.store(false, Ordering::SeqCst);
                    if self.pending_open.swap(false, Ordering::SeqCst) {
                        let _ = dsh::open_page();
                    }
                } else if self.pending_from_restart.swap(false, Ordering::SeqCst) {
                    // 重启失败：清除待办 (避免 dsh 恢复后反复自动打开无意义页面)，保留日志
                    if self.pending_open.swap(false, Ordering::SeqCst) {
                        log::warn("重启失败：已清除打开待办 (dsh 未就绪，watchdog 将保活重试)");
                    }
                }
                // "打开"来源的待办在失败时保留 (P0-4：等下次就绪再消费)，此处不处理
            }
            UserEvent::TooltipUpdate { ready } => {
                // 实时读 starting：事件到达时若 flow 已结束则显示最终状态，
                // 避免竞态 (watchdog 事件与 AnimationDone 乱序) 导致 tooltip 卡"启动中"
                self.update_tooltip(ready, self.starting.load(Ordering::SeqCst));
            }
        }
    }
}

/// 加载托盘图标：默认图标 (ICO 按 PNG 裁剪比例取中心 → 缩放 32x32)
/// + 启动动画帧 (扫描仪灯管来回扫动)
fn load_tray_icons() -> Result<Trayicons, Box<dyn Error>> {
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

        // 扫描仪灯管效果：中央不透明全白灯管 + 两侧线性衰减的半透明灯光。
        // 灯管中心在整个图标宽度内来回扫动 (三角波 ping-pong)，
        // 灯光溢出画布边缘的部分自然裁剪 (仅绘制画布内列)。
        const LAMP_W: u32 = 2; // 灯管宽度 (不透明全白)
        const LIGHT_W: i32 = 6; // 单侧灯光延伸宽度 (线性衰减)
        const LIGHT_MAX: i32 = 180; // 灯光最大强度 (灯管旁)
        let travel = 32 - LAMP_W; // 灯管中心移动范围：LAMP_W/2 ~ 32-LAMP_W/2
        let tri = (2.0 * t).fract();
        let wave = 1.0 - (2.0 * tri - 1.0).abs();
        let lamp_center = (LAMP_W as i32 / 2) + (wave * travel as f32) as i32;

        let half = LAMP_W as i32 / 2;
        // 逐列计算灯光强度并叠加 (alpha 混合，透明区域也能被灯光照亮)
        for x in 0..32u32 {
            let dist = (x as i32 - lamp_center).abs();
            let alpha = if dist <= half {
                255 // 灯管：不透明全白
            } else {
                let fall = dist - half;
                if fall < LIGHT_W {
                    LIGHT_MAX * (LIGHT_W - fall) / LIGHT_W
                } else {
                    0
                }
            };
            if alpha > 0 {
                let line = image::RgbaImage::from_pixel(1, 32, Rgba([255, 255, 255, alpha as u8]));
                image::imageops::overlay(&mut frame, &line, x as i64, 0);
            }
        }

        frames.push(Icon::from_rgba(frame.into_raw(), 32, 32)?);
    }

    Ok(Trayicons { default, frames })
}

fn main() -> Result<(), Box<dyn Error>> {
    // 单实例保护：已有 DshLauncher 在运行时，本实例直接退出；
    // 句柄存入 App，退出时提前释放以便新实例立即启动
    let mutex_handle = match dsh::single_instance_guard() {
        Some(handle) => Some(handle),
        None => return Ok(()),
    };

    // 托盘右键菜单：打开 / 配置 / 重启 / 退出 (自上而下)
    let menu = Menu::new();
    let open_item = MenuItem::new("打开", true, None);
    let config_item = MenuItem::new("配置", true, None);
    let restart_item = MenuItem::new("重启", true, None);
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
    };

    // 程序启动即让扫描灯流动；watchdog 首轮探测到 dsh 就绪后自动停止
    set_anim(&app.anim_running, true, &app.proxy);
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

    Ok(())
}
