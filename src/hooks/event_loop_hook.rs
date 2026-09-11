use crate::config::{read_config_or_recover, ImeMode};
use log::{debug, error, info, warn};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::Input::Ime::{
    ImmGetContext, ImmGetConversionStatus, ImmGetDefaultIMEWnd, ImmReleaseContext,
    ImmSetConversionStatus, IME_CONVERSION_MODE, IME_SENTENCE_MODE,
};
use windows::Win32::UI::{
    Accessibility::*, Input::KeyboardAndMouse::*, WindowsAndMessaging::*,
};

// 事件去抖：记录上次处理的前台窗口与时间，避免快速切窗时线程暴涨、日志刷屏
static LAST_EVENT_HWND: AtomicUsize = AtomicUsize::new(0);
static LAST_EVENT_MS: AtomicU64 = AtomicU64::new(0);

const EVENT_DEBOUNCE_MS: u64 = 500;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

pub unsafe extern "system" fn event_hook_callback(
    _h_win_event_hook: HWINEVENTHOOK,
    _event: u32,
    hwnd: HWND,
    _id_object: i32,
    _id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    let hwnd_raw = hwnd.0 as usize;
    let now = now_ms();

    // 去抖：同一窗口在去抖窗口期内的重复前台事件直接忽略
    let last_hwnd = LAST_EVENT_HWND.load(Ordering::Relaxed);
    let last_ms = LAST_EVENT_MS.load(Ordering::Relaxed);
    if hwnd_raw == last_hwnd && now.saturating_sub(last_ms) < EVENT_DEBOUNCE_MS {
        return;
    }
    LAST_EVENT_HWND.store(hwnd_raw, Ordering::Relaxed);
    LAST_EVENT_MS.store(now, Ordering::Relaxed);

    // 一次性读取所需配置（锁中毒时自动恢复）
    let (master_switch, ime_mode) = {
        let cfg = read_config_or_recover();
        (cfg.master_switch, cfg.ime_mode.clone())
    };
    if !master_switch {
        return;
    }

    // 在新线程中执行，避免阻塞事件钩子
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(100)); // 等待窗口稳定
        unsafe {
            let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);
            // 状态已符合目标模式（包括用户主动切换到 ENG 等非中文键盘）时不再强制
            if !is_ime_state_matching(hwnd, &ime_mode) {
                enforce_global_ime_mode(hwnd, &ime_mode);
            }
        }
    });
}

/// WM_TIMER 触发的周期性兜底检查：只有状态不符时才强制，正常情况下几乎零开销
pub unsafe fn handle_ime_check_timer() {
    let (master_switch, ime_mode) = {
        let cfg = read_config_or_recover();
        (cfg.master_switch, cfg.ime_mode.clone())
    };
    if !master_switch {
        return;
    }

    let hwnd = GetForegroundWindow();
    if hwnd.is_invalid() {
        return;
    }

    if !is_ime_state_matching(hwnd, &ime_mode) {
        debug!("定时检测发现输入法状态不符（目标: {:?}），执行强制", ime_mode);
        enforce_global_ime_mode(hwnd, &ime_mode);
    }
}

// WM_IME_CONTROL 消息与子命令（windows crate 未导出，手动定义，数值出自 imm.h）
const WM_IME_CONTROL: u32 = 0x0283;
const IMC_GETOPENSTATUS: usize = 0x0002;
const IMC_SETOPENSTATUS: usize = 0x0003;
const IMC_GETCONVERSIONMODE: usize = 0x0005;
const IMC_SETCONVERSIONMODE: usize = 0x0006;

// ===== 失败日志节流（同类失败 30 秒最多一条，避免 1.5 秒检测周期刷屏） =====
static DEFIME_FAIL_MS: AtomicU64 = AtomicU64::new(0);
static SHIFT_FAIL_MS: AtomicU64 = AtomicU64::new(0);
static MISMATCH_LOG_MS: AtomicU64 = AtomicU64::new(0);
const FAIL_LOG_THROTTLE_MS: u64 = 30_000;

fn throttled_warn(cell: &AtomicU64, msg: &str) {
    let now = now_ms();
    let last = cell.load(Ordering::Relaxed);
    if now.saturating_sub(last) >= FAIL_LOG_THROTTLE_MS
        && cell
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        warn!("{}", msg);
    }
}

// ===== SendInput Shift 兜底 =====

/// 是否有修饰键正被按住（避免模拟 Shift 干扰用户的组合键操作）
unsafe fn any_modifier_pressed() -> bool {
    const PRESSED_MASK: i16 = i16::MIN; // GetAsyncKeyState 最高位 = 按下中
    [
        VK_SHIFT,
        VK_LSHIFT,
        VK_RSHIFT,
        VK_CONTROL,
        VK_LCONTROL,
        VK_RCONTROL,
        VK_MENU,
        VK_LMENU,
        VK_RMENU,
        VK_LWIN,
        VK_RWIN,
    ]
    .iter()
    .any(|k| GetAsyncKeyState(k.0 as i32) & PRESSED_MASK != 0)
}

/// 模拟一次 Shift 按下+抬起：微软拼音默认 Shift 切换中英，可借此把"英"弹回"中"
unsafe fn send_shift_tap() -> bool {
    if any_modifier_pressed() {
        throttled_warn(&SHIFT_FAIL_MS, "检测到修饰键按住，跳过 Shift 模拟");
        return false;
    }
    let down = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_SHIFT,
                wScan: 0,
                dwFlags: KEYBD_EVENT_FLAGS(0),
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let up = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_SHIFT,
                wScan: 0,
                dwFlags: KEYEVENTF_KEYUP,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    SendInput(&[down, up], std::mem::size_of::<INPUT>() as i32) == 2
}

// ===== 统一的转换模式查询（检测路径，多级来源） =====

/// 按优先级查询当前前台窗口的转换模式，返回 (cmode, 来源通道名)
unsafe fn query_current_cmode(hwnd: HWND) -> Option<(u32, &'static str)> {
    // 1) IMM32 直查（最精确，绑定窗口上下文）
    let himc = ImmGetContext(hwnd);
    if !himc.0.is_null() {
        let mut cmode = IME_CONVERSION_MODE::default();
        let mut smode = IME_SENTENCE_MODE::default();
        let ok = ImmGetConversionStatus(himc, Some(&mut cmode), Some(&mut smode)).as_bool();
        let _ = ImmReleaseContext(hwnd, himc);
        if ok {
            return Some((cmode.0, "IMM32"));
        }
    }
    // 2) DefIMEWnd（纯 TSF 窗口的主读取通道，实测能反映微软拼音真实子模式）
    if let Some(v) = get_conversion_mode_via_defime(hwnd) {
        return Some((v, "DefIMEWnd"));
    }
    None
}

/// 前台窗口上下文描述（诊断日志用）：窗口类名 [标题] 进程ID
unsafe fn foreground_context(hwnd: HWND) -> String {
    let mut cls_buf = [0u16; 128];
    let cls_len = GetClassNameW(hwnd, &mut cls_buf);
    let cls = if cls_len > 0 {
        String::from_utf16_lossy(&cls_buf[..cls_len as usize])
    } else {
        "?".to_string()
    };
    let mut title_buf = [0u16; 128];
    let title_len = GetWindowTextW(hwnd, &mut title_buf);
    let title = if title_len > 0 {
        String::from_utf16_lossy(&title_buf[..title_len as usize])
    } else {
        String::new()
    };
    let mut pid = 0u32;
    let _ = GetWindowThreadProcessId(hwnd, Some(&mut pid));
    format!("{} [{}] pid={}", cls, title, pid)
}

/// 通过"默认 IME 窗口"（DefIMEWnd，由微软拼音 TSF 兼容层维护）查询全局转换模式。
///
/// 纯 TSF 窗口（Electron/Chromium/UWP 等）上 ImmGetContext 返回空，
/// 这条通道是读取/设置中英状态唯一可靠的兜底手段。
unsafe fn get_conversion_mode_via_defime(hwnd: HWND) -> Option<u32> {
    let ime_wnd = ImmGetDefaultIMEWnd(hwnd);
    if ime_wnd.is_invalid() || ime_wnd.0.is_null() {
        return None;
    }
    let ret = SendMessageW(
        ime_wnd,
        WM_IME_CONTROL,
        Some(WPARAM(IMC_GETCONVERSIONMODE)),
        Some(LPARAM(0)),
    );
    Some(ret.0 as u32)
}

/// 通过默认 IME 窗口设置转换模式（并确保 IME 处于打开状态）。
/// 返回 true 表示设置命令被接受。
unsafe fn set_conversion_mode_via_defime(hwnd: HWND, new_cmode: u32) -> bool {
    let ime_wnd = ImmGetDefaultIMEWnd(hwnd);
    if ime_wnd.is_invalid() || ime_wnd.0.is_null() {
        return false;
    }
    // 部分场景"英"对应 IME 关闭，先确保 IME 打开再设转换模式
    SendMessageW(ime_wnd, WM_IME_CONTROL, Some(WPARAM(IMC_SETOPENSTATUS)), Some(LPARAM(1)));
    let ret = SendMessageW(
        ime_wnd,
        WM_IME_CONTROL,
        Some(WPARAM(IMC_SETCONVERSIONMODE)),
        Some(LPARAM(new_cmode as isize)),
    );
    ret.0 != 0
}

/// 判断前台窗口当前的输入法状态是否符合目标模式
///
/// 注意：ChineseOnly 模式的定位是"约束中文 IME 的英文子模式"（如微软拼音的英/中切换），
/// 用户主动切换到其他键盘（如 ENG 美式英语键盘）属于合法状态，不应被纠正回中文 IME。
unsafe fn is_ime_state_matching(hwnd: HWND, ime_mode: &ImeMode) -> bool {
    // 检查前台窗口所属线程的键盘布局语言 ID
    let tid = GetWindowThreadProcessId(hwnd, None);
    let hkl = GetKeyboardLayout(tid);
    let current_lang = (hkl.0 as usize) & 0xFFFF;

    match ime_mode {
        ImeMode::ChineseOnly => {
            // 当前激活的不是中文键盘（如 ENG 美式键盘）：用户自主选择，视为符合，不强制
            if current_lang != 0x0804 {
                return true;
            }

            // 当前是中文 IME：校验转换状态（NATIVE 位 = 中文输入模式）
            match query_current_cmode(hwnd) {
                Some((cmode, src)) => {
                    let native = cmode & 0x0001 != 0;
                    if !native {
                        throttled_warn(
                            &MISMATCH_LOG_MS,
                            &format!(
                                "检测到非中文子模式: 来源={} cmode=0x{:08x} 窗口={}",
                                src,
                                cmode,
                                foreground_context(hwnd)
                            ),
                        );
                    }
                    native // IME_CMODE_NATIVE（与 IME_CMODE_CHINESE 同值）
                }
                None => true, // 所有读取通道都不可用，只能放行
            }
        }
        ImeMode::EnglishOnly => current_lang == 0x0409,
    }
}

/// 从已安装布局中动态解析目标语言的真实 HKL，避免写死 0x0804/0x0409 强制错布局
unsafe fn resolve_target_hkl(ime_mode: &ImeMode) -> HKL {
    let target_lang: usize = match ime_mode {
        ImeMode::ChineseOnly => 0x0804,
        ImeMode::EnglishOnly => 0x0409,
    };

    let count = GetKeyboardLayoutList(None);
    if count > 0 {
        let mut layouts: Vec<HKL> = Vec::with_capacity(count as usize);
        layouts.resize_with(count as usize, || HKL(std::ptr::null_mut()));
        let loaded = GetKeyboardLayoutList(Some(&mut layouts));

        // 优先取"高字非 0"的布局（通常是真正的 IME 布局），其次取任意匹配语言 ID 的布局
        let mut fallback: Option<HKL> = None;
        for hkl in layouts.into_iter().take(loaded.max(0) as usize) {
            let raw = hkl.0 as usize;
            if raw & 0xFFFF == target_lang {
                if raw >> 16 != 0 {
                    return hkl;
                }
                if fallback.is_none() {
                    fallback = Some(hkl);
                }
            }
        }
        if let Some(hkl) = fallback {
            return hkl;
        }
    }

    debug!(
        "未找到语言 ID 0x{:04x} 的已安装布局，回退到默认 HKL",
        target_lang
    );
    HKL(target_lang as *mut std::ffi::c_void)
}

/// 通过 IMM32 设置前台窗口的输入转换状态，真正锁住"中文/英文输入模式"
/// （即使布局切换请求被目标窗口忽略，输入模式也能被纠正）
unsafe fn apply_ime_conversion_status(hwnd: HWND, ime_mode: &ImeMode) {
    let set_native = matches!(ime_mode, ImeMode::ChineseOnly);

    // 通道1: IMM32 直设（绑定窗口上下文，最精确）
    let himc = ImmGetContext(hwnd);
    if !himc.0.is_null() {
        let mut cmode = IME_CONVERSION_MODE::default();
        let mut smode = IME_SENTENCE_MODE::default();
        if ImmGetConversionStatus(himc, Some(&mut cmode), Some(&mut smode)).as_bool() {
            let new_cmode = if set_native {
                // IME_CMODE_NATIVE（= IME_CMODE_CHINESE，0x0001）标志中文输入模式
                IME_CONVERSION_MODE(cmode.0 | 0x0001)
            } else {
                IME_CONVERSION_MODE(cmode.0 & !0x0001)
            };
            if ImmSetConversionStatus(himc, new_cmode, smode).as_bool() {
                debug!("输入转换状态已通过 IMM32 设置: {:?}", ime_mode);
                let _ = ImmReleaseContext(hwnd, himc);
                return;
            }
        }
        let _ = ImmReleaseContext(hwnd, himc);
    }

    // 通道2: DefIMEWnd（写入常被微软拼音 TSF 拒绝，保留尝试）
    if let Some(cmode) = get_conversion_mode_via_defime(hwnd) {
        let new_cmode = if set_native {
            cmode | 0x0001
        } else {
            cmode & !0x0001
        };
        if set_conversion_mode_via_defime(hwnd, new_cmode) {
            info!(
                "输入转换状态已通过默认 IME 窗口设置: {:?}（窗口 {}）",
                ime_mode,
                foreground_context(hwnd)
            );
            return;
        }
        throttled_warn(&DEFIME_FAIL_MS, "DefIMEWnd 设置转换状态被拒绝（微软拼音 TSF 常见行为）");
    } else {
        throttled_warn(&DEFIME_FAIL_MS, "DefIMEWnd 不可用，跳过该通道");
    }

    // 通道3: 模拟 Shift（仅锁中文时使用，利用微软拼音 Shift 切中英特性；端到端实测有效）
    if set_native {
        if send_shift_tap() {
            info!(
                "已通过模拟 Shift 键弹回中文: {:?}（窗口 {}）",
                ime_mode,
                foreground_context(hwnd)
            );
        } else {
            throttled_warn(&SHIFT_FAIL_MS, "模拟 Shift 未执行或发送失败");
        }
    } else {
        throttled_warn(&SHIFT_FAIL_MS, "所有转换状态设置通道均失败（EnglishOnly 无 Shift 兜底）");
    }
}

pub unsafe fn enforce_global_ime_mode(hwnd: HWND, ime_mode: &ImeMode) {
    // 动态解析目标键盘布局
    let target_hkl = resolve_target_hkl(ime_mode);

    // 方法1: 激活目标键盘布局（全局）
    let result = ActivateKeyboardLayout(
        target_hkl,
        ACTIVATE_KEYBOARD_LAYOUT_FLAGS(KLF_ACTIVATE.0 | KLF_SETFORPROCESS.0 | KLF_REORDER.0),
    );

    match result {
        Ok(_) => {
            debug!("ActivateKeyboardLayout 成功");

            // 发送消息确保UI更新
            let _ = PostMessageW(
                Some(HWND_BROADCAST),
                WM_INPUTLANGCHANGEREQUEST,
                WPARAM(0),
                LPARAM(target_hkl.0 as isize),
            );
        }
        Err(_) => {
            debug!("ActivateKeyboardLayout 失败，尝试备用方法");

            // 方法2: 加载并激活键盘布局
            let layout_name = match ime_mode {
                ImeMode::ChineseOnly => "00000804",
                ImeMode::EnglishOnly => "00000409",
            };

            let wide_layout: Vec<u16> = layout_name
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();

            if let Ok(loaded_hkl) = LoadKeyboardLayoutW(PCWSTR(wide_layout.as_ptr()), KLF_ACTIVATE)
            {
                if !loaded_hkl.0.is_null() {
                    // 再次尝试激活
                    if ActivateKeyboardLayout(loaded_hkl, KLF_ACTIVATE).is_ok() {
                        debug!("通过 LoadKeyboardLayout 激活成功");

                        // 发送全局消息确保UI更新
                        let _ = PostMessageW(
                            Some(HWND_BROADCAST),
                            WM_INPUTLANGCHANGEREQUEST,
                            WPARAM(0),
                            LPARAM(loaded_hkl.0 as isize),
                        );
                    } else {
                        error!("LoadKeyboardLayout 后激活失败");
                    }
                }
            } else {
                error!("LoadKeyboardLayout 失败");
            }
        }
    }

    // 方法3: 通过 IMM32 锁定输入转换状态（中/英模式）
    apply_ime_conversion_status(hwnd, ime_mode);

    // 方法4: 发送设置变更消息（总是执行）
    let _ = PostMessageW(Some(HWND_BROADCAST), WM_SETTINGCHANGE, WPARAM(0), LPARAM(0));

    debug!("全局输入法强制设置完成: {:?}", ime_mode);

    // 验证当前布局（仅在不一致时告警，避免日志刷屏）
    verify_current_layout(ime_mode);
}

unsafe fn verify_current_layout(ime_mode: &ImeMode) {
    // 获取当前线程的键盘布局
    let current_hkl = GetKeyboardLayout(0); // 0表示当前线程
    let target_lang: usize = match ime_mode {
        ImeMode::ChineseOnly => 0x0804,
        ImeMode::EnglishOnly => 0x0409,
    };
    let current_lang = (current_hkl.0 as usize) & 0xFFFF;

    debug!(
        "验证 - 当前HKL: 0x{:x} (语言 0x{:04x}), 目标语言: 0x{:04x}",
        current_hkl.0 as usize, current_lang, target_lang
    );

    if current_lang != target_lang {
        warn!(
            "输入法布局验证不一致: 当前语言 0x{:04x}, 目标语言 0x{:04x}",
            current_lang, target_lang
        );
    }
}
