use crate::config::{
    apply_ime_setting_to_current_window, read_config_or_recover, write_config_or_recover, ImeMode,
};
use crate::constants;
use crate::tray::icon;
use crate::tray::notifications::show_balloon_tip;
use log::info;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};

pub fn handle_hotkey(hwnd: HWND, wparam: WPARAM) {
    match wparam.0 as i32 {
        constants::HOT_KEY_TOGGLE_ID => {
            info!("热键触发: 切换总开关");
            let new_state = {
                let mut config = write_config_or_recover();
                config.master_switch = !config.master_switch;
                config.save().ok();
                config.master_switch
            };

            let _ = icon::update_tray_icon(hwnd, new_state);

            let msg = if new_state {
                "已启用强制输入法模式"
            } else {
                "已恢复输入法自动切换"
            };

            if read_config_or_recover().show_notifications {
                show_balloon_tip(hwnd, "快捷键触发", msg);
            }
        }
        constants::HOT_KEY_SWITCH_MODE_ID => {
            info!("热键触发: 切换输入法模式");
            let new_mode = {
                let mut config = write_config_or_recover();
                let new_mode = match config.ime_mode {
                    ImeMode::ChineseOnly => ImeMode::EnglishOnly,
                    ImeMode::EnglishOnly => ImeMode::ChineseOnly,
                };
                config.ime_mode = new_mode.clone();
                config.save().ok();
                new_mode
            };
            apply_ime_setting_to_current_window(hwnd, new_mode);
        }
        _ => {}
    }
}
