//! StatusNotifierItem and D-Bus menu presentation. Some AppIndicator hosts
//! open the menu with AboutToShow alone, without an icon activation event.
use super::{Event, EventSender, menu};
use anyhow::Result;
use ksni::{
    blocking::{Handle, TrayMethods},
    menu::{CheckmarkItem, StandardItem, SubMenu},
};
use tray_icon::menu::{ContextMenu, Menu, MenuItemKindSnapshot as Item, MenuSnapshotHandle};

pub struct LinuxTray {
    handle: Handle<Indicator>,
    menu: Menu,
}

impl LinuxTray {
    pub fn new(sender: EventSender) -> Result<Self> {
        let menu = Menu::new();
        let indicator = Indicator {
            sender,
            menu: menu.snapshot_handle(),
            icon: icon(false),
            tooltip: "Noria · loading".into(),
        };
        let sandboxed = std::path::Path::new("/.flatpak-info").exists()
            || std::env::var_os("SNAP").is_some()
            || std::fs::read_to_string("/proc/self/cgroup").is_ok_and(|groups| {
                groups.lines().any(|line| {
                    line.rsplit('/')
                        .next()
                        .is_some_and(|unit| unit.starts_with("snap."))
                })
            });
        let handle = indicator
            .assume_sni_available(true)
            .disable_dbus_name(sandboxed)
            .spawn()?;
        Ok(Self { handle, menu })
    }

    pub fn render(&mut self, menu: Menu, running: bool, tooltip: &str) {
        let snapshot = menu.snapshot_handle();
        self.handle.update(|indicator| {
            indicator.menu = snapshot;
            indicator.icon = icon(running);
            indicator.tooltip = tooltip.to_owned();
        });
        self.menu = menu;
    }
}

impl Drop for LinuxTray {
    fn drop(&mut self) {
        self.handle.shutdown().wait();
    }
}

struct Indicator {
    sender: EventSender,
    menu: MenuSnapshotHandle,
    icon: ksni::Icon,
    tooltip: String,
}

impl ksni::Tray for Indicator {
    fn id(&self) -> String {
        "noria".into()
    }
    fn title(&self) -> String {
        "Noria".into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![self.icon.clone()]
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: self.title(),
            description: self.tooltip.clone(),
            icon_pixmap: self.icon_pixmap(),
            ..Default::default()
        }
    }
    fn activate(&mut self, _: i32, _: i32) {
        self.sender.send(Event::Refresh);
    }
    fn secondary_activate(&mut self, _: i32, _: i32) {
        self.sender.send(Event::Refresh);
    }
    fn menu_about_to_show(&mut self) {
        self.sender.send(Event::Refresh);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        menu_items(&self.menu.items())
    }
}

fn icon(running: bool) -> ksni::Icon {
    let mut data = menu::icon_rgba(running);
    for pixel in data.as_chunks_mut::<4>().0 {
        pixel.rotate_right(1); // D-Bus pixmaps use network-order ARGB.
    }
    ksni::Icon {
        width: 48,
        height: 48,
        data,
    }
}

// Noria uses plain labels, including user-provided account aliases. Escape
// underscores so D-Bus menu clients display them as text, not mnemonics.
fn menu_items(items: &[Item]) -> Vec<ksni::MenuItem<Indicator>> {
    items
        .iter()
        .filter_map(|item| {
            Some(match item {
                Item::MenuItem(item) => {
                    let activate = item.activate.clone();
                    StandardItem {
                        label: item.text().replace('_', "__"),
                        enabled: item.is_enabled(),
                        activate: Box::new(move |_| activate()),
                        ..Default::default()
                    }
                    .into()
                }
                Item::Check(item) => {
                    let activate = item.activate.clone();
                    CheckmarkItem {
                        label: item.text().replace('_', "__"),
                        enabled: item.is_enabled(),
                        checked: item.is_checked(),
                        activate: Box::new(move |_| activate()),
                        ..Default::default()
                    }
                    .into()
                }
                Item::Submenu(item) => SubMenu {
                    label: item.text().replace('_', "__"),
                    enabled: item.is_enabled(),
                    submenu: menu_items(&item.items()),
                    ..Default::default()
                }
                .into(),
                Item::Predefined(item) if item.is_separator() => ksni::MenuItem::Separator,
                _ => return None,
            })
        })
        .collect()
}
