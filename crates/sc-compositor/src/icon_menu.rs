//! The context menu a long press on a home or dock icon opens.
//!
//! Tapping an icon raises the app it belongs to, which leaves nowhere to ask for
//! a *second* window — the reason two terminals, or two PWAs, used to be
//! unreachable. This menu is that "nowhere": it holds everything that is not a
//! plain open.
//!
//! The rows an app gets depend on what it is doing, so they are built per open
//! rather than fixed: a stopped app can only be started, a running one can be
//! raised or closed, and one with several windows lists them by title so the
//! right one can be picked directly. Geometry is [`sc_layout::menu`].

use crate::ui_state::ToplevelId;

/// What a menu row does when tapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuAction {
    /// Raise this window. One row per window once an app has more than one;
    /// with a single window it is the lone "Open" row.
    Open(ToplevelId),
    /// Start another instance, whether or not one is already running.
    NewWindow,
    /// Ask every window of this app to close.
    CloseAll,
    /// Take the app off the home screen (same edit as the arrange remove badge).
    Remove,
    /// Put a library app on the home screen, without making the user drag it.
    AddToHome,
    /// Flatpak apps only: arm the confirm row. Does not uninstall anything.
    Uninstall,
    /// Actually run `flatpak uninstall`.
    UninstallConfirm,
}

impl MenuAction {
    /// Whether the row reads as destructive (drawn in a warning tint).
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

/// One laid-out row: what it does and what it says.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MenuItem {
    pub action: MenuAction,
    pub label: String,
}

/// The rows for an icon, given its windows as `(toplevel, title)` in MRU order.
///
/// A single window gets a plain "Open" — its title would just repeat the app
/// name under the icon the finger is already on. Several windows are listed
/// individually, because picking between them is the only reason to look.
/// `in_library` swaps the "Remove" row for "Add to Home": the menu was opened
/// on a folder member, which is by definition not on a page to remove it from.
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
            // A client that never set a title still needs a distinguishable
            // row, and its position in the MRU order is the one thing we always
            // know about it.
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

/// The rows the Uninstall row swaps in: deleting an app is not a thing a single
/// stray tap should be able to do.
fn confirm_items(name: &str) -> Vec<MenuItem> {
    vec![MenuItem {
        action: MenuAction::UninstallConfirm,
        label: format!("Delete {name}?"),
    }]
}

/// An open icon menu.
pub(crate) struct IconMenu {
    pub app_id: String,
    /// Icon center the panel is anchored to (output pixels).
    pub anchor: (f32, f32),
    pub items: Vec<MenuItem>,
    /// Row under the finger, for the pressed highlight.
    pub pressed: Option<usize>,
    /// 0→1 open animation.
    pub open: sc_anim::Spring,
}

impl IconMenu {
    pub(crate) fn new(app_id: String, anchor: (f32, f32), items: Vec<MenuItem>) -> Self {
        // Snappy: the panel should feel like it was already there by the time
        // the finger lifts off the hold that opened it.
        let open = sc_anim::Spring::zoom(0.0, 1.0);
        Self {
            app_id,
            anchor,
            items,
            pressed: None,
            open,
        }
    }

    /// Panel + row rects for the current output size.
    pub(crate) fn layout(&self, width: f32, height: f32) -> sc_layout::menu::MenuLayout {
        sc_layout::menu::compute(self.anchor, self.items.len(), width, height)
    }
}

impl crate::state::State {
    /// The flatpak ref backing `app_id`, if it came from one.
    pub(crate) fn flatpak_ref(&self, app_id: &str) -> Option<&str> {
        self.app_catalog.get(app_id)?.flatpak.as_deref()
    }

    pub(crate) fn is_flatpak(&self, app_id: &str) -> bool {
        self.flatpak_ref(app_id).is_some()
    }

    /// Carry out a menu row. The menu itself has already been closed.
    pub(crate) fn run_menu_action(&mut self, menu: &IconMenu, action: MenuAction) {
        let app_id = menu.app_id.clone();
        tracing::debug!(
            target: "springchick::debug",
            "icon menu action app_id={app_id} action={action:?}"
        );
        // Zoom from the icon the menu belongs to, so an app opened from a menu
        // grows out of the same place a tap would have grown it from.
        let origin = crate::ui_state::ZoomOrigin::icon(menu.anchor);
        match action {
            // Raise the picked window by id rather than by app id: with several
            // open, "the app's most recent window" is exactly what the user is
            // choosing *against*.
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
            // Reopen on the same anchor with only the confirm row, so the
            // destructive tap is never the one the finger is already making.
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

    /// Uninstall the flatpak behind `app_id`. The icon stays until the child
    /// exits and `poll_launching` rescans the catalog.
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

    /// A client that set no title still needs a row that can be told apart from
    /// its siblings.
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
