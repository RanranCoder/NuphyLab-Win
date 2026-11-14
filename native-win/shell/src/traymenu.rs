// Tray icon + menu, mirroring the NSStatusItem menu in AppHost.buildMenus():
// Open / Pause-Resume / Stop and Restore Lighting / Start at Login / Quit.
use tao::event_loop::EventLoopProxy;
use tray_icon::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tray_icon::{TrayIcon, TrayIconBuilder, TrayIconEvent};

#[derive(Clone, Copy, Debug)]
pub enum TrayAction {
    Show,
    TogglePause,
    Restore,
    ToggleAutostart,
    Quit,
}

pub const ID_SHOW: &str = "show";
pub const ID_PAUSE: &str = "pause";
pub const ID_RESTORE: &str = "restore";
pub const ID_AUTOSTART: &str = "autostart";
pub const ID_QUIT: &str = "quit";

fn menu(english: bool, autostart: bool) -> Menu {
    let label = |zh: &'static str, en: &'static str| -> &'static str { if english { en } else { zh } };
    let menu = Menu::new();
    let _ = menu.append_items(&[
        &MenuItem::with_id(ID_SHOW, label("打开 NuPhyLab", "Open NuPhyLab"), true, None),
        &MenuItem::with_id(ID_PAUSE, label("暂停 / 继续", "Pause / Resume"), true, None),
        &MenuItem::with_id(ID_RESTORE, label("停止并恢复键盘灯效", "Stop and Restore Lighting"), true, None),
        &PredefinedMenuItem::separator(),
        &CheckMenuItem::with_id(ID_AUTOSTART, label("开机自动启动", "Start at Login"), true, autostart, None),
        &MenuItem::with_id(ID_QUIT, label("退出 NuPhyLab", "Quit NuPhyLab"), true, None),
    ]);
    menu
}

pub struct Tray {
    icon: TrayIcon,
    english: bool,
    autostart: bool,
}

impl Tray {
    pub fn build(icon: tray_icon::Icon, english: bool, autostart: bool, proxy: EventLoopProxy<crate::UserEvent>) -> Tray {
        let icon = TrayIconBuilder::new()
            .with_id("main")
            .with_tooltip("NuPhyLab")
            .with_icon(icon)
            .with_menu(Box::new(menu(english, autostart)))
            // Windows convention: left click opens the window, right click shows the menu.
            .with_menu_on_left_click(false)
            .build()
            .expect("tray icon");
        let proxy_click = proxy.clone();
        std::thread::spawn(move || {
            while let Ok(event) = tray_icon::menu::MenuEvent::receiver().recv() {
                let action = match event.id.0.as_str() {
                    ID_SHOW => TrayAction::Show,
                    ID_PAUSE => TrayAction::TogglePause,
                    ID_RESTORE => TrayAction::Restore,
                    ID_AUTOSTART => TrayAction::ToggleAutostart,
                    ID_QUIT => TrayAction::Quit,
                    _ => continue,
                };
                if proxy.send_event(crate::UserEvent::Tray(action)).is_err() {
                    break;
                }
            }
        });
        std::thread::spawn(move || {
            while let Ok(event) = TrayIconEvent::receiver().recv() {
                if let TrayIconEvent::Click { button: tray_icon::MouseButton::Left, .. } = event {
                    if proxy_click.send_event(crate::UserEvent::Tray(TrayAction::Show)).is_err() {
                        break;
                    }
                }
            }
        });
        Tray { icon, english, autostart }
    }

    fn rebuild(&mut self, english: bool, autostart: bool) {
        if english == self.english && autostart == self.autostart {
            return;
        }
        self.english = english;
        self.autostart = autostart;
        self.icon.set_menu(Some(Box::new(menu(english, autostart))));
    }

    pub fn set_language(&mut self, english: bool) {
        self.rebuild(english, self.autostart);
    }

    pub fn set_autostart(&mut self, enabled: bool) {
        self.rebuild(self.english, enabled);
    }
}
