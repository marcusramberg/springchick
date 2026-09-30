//! The long-press icon menu. Rows depend on app state: start, raise, close,
//! or pick one of several windows by title. Geometry is [`sc_layout::menu`].

use crate::ui_state::ToplevelId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuAction {
    /// One row per window when there are several; otherwise the lone "Open".
    Open(ToplevelId),
    NewWindow,
    CloseAll,
    Remove,
    AddToHome,
    /// Flatpak only: arm the confirm row.
    Uninstall,
    UninstallConfirm,
}

impl MenuAction {
    pub(crate) fn is_destructive(self) -> bool {
        matches!(
            self,
            MenuAction::CloseAll
                | MenuAction::Remove
                | MenuAction::Uninstall
                | MenuAction::UninstallConfirm
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MenuItem {
    pub action: MenuAction,
    pub label: String,
}

/// Rows for an icon, `windows` in MRU order. One window gets a plain "Open";
/// several are listed by title. `in_library` swaps "Remove" for "Add to Home".
pub(crate) fn items_for(
    windows: &[(ToplevelId, String)],
    flatpak: bool,
    in_library: bool,
) -> Vec<MenuItem> {
    let mut items = Vec::with_capacity(windows.len() + 4);
    match windows {
        [] => {}
        [(id, _)] => items.push(MenuItem {
            action: MenuAction::Open(*id),
            label: "Open".into(),
        }),
        many => items.extend(many.iter().enumerate().map(|(i, (id, title))| MenuItem {
            action: MenuAction::Open(*id),
            label: if title.is_empty() {
                format!("Window {}", i + 1)
            } else {
                title.clone()
            },
        })),
    }
    items.push(MenuItem {
        action: MenuAction::NewWindow,
        label: "New window".into(),
    });
    if !windows.is_empty() {
        items.push(MenuItem {
            action: MenuAction::CloseAll,
            label: if windows.len() > 1 {
                "Close all".into()
            } else {
                "Close".into()
            },
        });
    }
    items.push(if in_library {
        MenuItem {
            action: MenuAction::AddToHome,
            label: "Add to Home".into(),
        }
    } else {
        MenuItem {
            action: MenuAction::Remove,
            label: "Remove".into(),
        }
    });
    if flatpak {
        items.push(MenuItem {
            action: MenuAction::Uninstall,
            label: "Uninstall".into(),
        });
    }
    items
}

/// Swapped in by Uninstall so one stray tap can't delete an app.
fn confirm_items(name: &str) -> Vec<MenuItem> {
    vec![MenuItem {
        action: MenuAction::UninstallConfirm,
        label: format!("Delete {name}?"),
    }]
}

pub(crate) struct IconMenu {
    pub app_id: String,
    /// Output pixels.
    pub anchor: (f32, f32),
    pub items: Vec<MenuItem>,
    pub pressed: Option<usize>,
    pub open: sc_anim::Spring,
}

impl IconMenu {
    pub(crate) fn new(app_id: String, anchor: (f32, f32), items: Vec<MenuItem>) -> Self {
        let open = sc_anim::Spring::zoom(0.0, 1.0);
        Self {
            app_id,
            anchor,
            items,
            pressed: None,
            open,
        }
    }

    pub(crate) fn layout(&self, width: f32, height: f32) -> sc_layout::menu::MenuLayout {
        sc_layout::menu::compute(self.anchor, self.items.len(), width, height)
    }
}

impl crate::state::State {
    pub(crate) fn flatpak_ref(&self, app_id: &str) -> Option<&str> {
        self.app_catalog.get(app_id)?.flatpak.as_deref()
    }

    pub(crate) fn is_flatpak(&self, app_id: &str) -> bool {
        self.flatpak_ref(app_id).is_some()
    }

    /// The menu has already been closed.
    pub(crate) fn run_menu_action(&mut self, menu: &IconMenu, action: MenuAction) {
        let app_id = menu.app_id.clone();
        tracing::debug!(
            target: "springchick::debug",
            "icon menu action app_id={app_id} action={action:?}"
        );
        let origin = crate::ui_state::ZoomOrigin::icon(menu.anchor);
        match action {
            // By toplevel id: with several windows, "most recent" is what the user is
            // choosing against.
            MenuAction::Open(id) => {
                self.last_origin = origin;
                self.raise_toplevel(id, origin);
            }
            MenuAction::NewWindow => self.spawn_instance(&app_id, origin),
            MenuAction::CloseAll => self.close_all(&app_id),
            MenuAction::Remove => {
                self.model.delete(&app_id);
                self.after_arrange_edit();
            }
            MenuAction::AddToHome => {
                self.model.place(app_id.clone());
                self.close_folder();
                self.after_arrange_edit();
            }
            // Reopen with only the confirm row, so the finger's current tap can't
            // confirm.
            MenuAction::Uninstall => {
                let name = self
                    .app_catalog
                    .get(&app_id)
                    .map(|e| e.name.clone())
                    .unwrap_or_else(|| app_id.clone());
                self.icon_menu = Some(IconMenu::new(app_id, menu.anchor, confirm_items(&name)));
            }
            MenuAction::UninstallConfirm => self.uninstall_flatpak(&app_id),
        }
        self.needs_render = true;
    }

    fn close_all(&mut self, app_id: &str) {
        for id in self.instances(app_id) {
            self.detach_toplevel(id);
            crate::ui_state::transition(
                &mut self.ui,
                crate::ui_state::UiEvent::ToplevelClosed {
                    toplevel: id,
                    next: None,
                },
            );
        }
    }

    /// The icon stays until the child exits and `poll_launching` rescans.
    fn uninstall_flatpak(&mut self, app_id: &str) {
        let Some(reference) = self.flatpak_ref(app_id).map(str::to_string) else {
            return;
        };
        // flatpak refuses to remove a running app.
        self.close_all(app_id);
        tracing::info!(app_id, reference, "uninstalling flatpak");
        match std::process::Command::new("flatpak")
            .args(["uninstall", "--noninteractive", &reference])
            .spawn()
        {
            Ok(child) => self.uninstalling.push(child),
            Err(e) => tracing::warn!(%e, reference, "failed to run flatpak uninstall"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(items: &[MenuItem]) -> Vec<&str> {
        items.iter().map(|i| i.label.as_str()).collect()
    }

    #[test]
    fn a_stopped_app_can_only_be_started_or_removed() {
        assert_eq!(
            labels(&items_for(&[], false, false)),
            ["New window", "Remove"]
        );
    }

    #[test]
    fn a_single_window_gets_a_plain_open() {
        let items = items_for(&[(3, "some terminal".into())], false, false);
        assert_eq!(labels(&items), ["Open", "New window", "Close", "Remove"]);
        assert_eq!(items[0].action, MenuAction::Open(3));
    }

    #[test]
    fn several_windows_are_listed_by_title_in_mru_order() {
        let items = items_for(&[(7, "notes.md".into()), (2, "~/src".into())], false, false);
        assert_eq!(
            labels(&items),
            ["notes.md", "~/src", "New window", "Close all", "Remove"]
        );
        assert_eq!(items[0].action, MenuAction::Open(7));
        assert_eq!(items[1].action, MenuAction::Open(2));
    }

    #[test]
    fn untitled_windows_fall_back_to_their_position() {
        let items = items_for(&[(7, String::new()), (2, String::new())], false, false);
        assert_eq!(labels(&items)[..2], ["Window 1", "Window 2"]);
    }

    #[test]
    fn only_flatpak_apps_offer_uninstall() {
        assert_eq!(
            labels(&items_for(&[], true, false)),
            ["New window", "Remove", "Uninstall"]
        );
        assert!(!labels(&items_for(&[], false, false)).contains(&"Uninstall"));
    }

    #[test]
    fn a_library_member_offers_add_to_home_instead_of_remove() {
        let items = items_for(&[], false, true);
        assert_eq!(labels(&items), ["New window", "Add to Home"]);
        assert!(!items.iter().any(|i| i.action.is_destructive()));
    }

    #[test]
    fn uninstall_needs_a_second_tap() {
        let confirm = confirm_items("Maps");
        assert_eq!(labels(&confirm), ["Delete Maps?"]);
        assert_eq!(confirm[0].action, MenuAction::UninstallConfirm);
        assert!(MenuAction::Uninstall.is_destructive());
    }
}
