use std::sync::Arc;

use client::UserStore;
use cloud_api_types::Organization;
use editor::Editor;
use gpui::{AppContext as _, DismissEvent, Entity, EventEmitter, Focusable, Render, WeakEntity};
use ui::prelude::*;
use workspace::{ModalView, Workspace, notifications::NotifyTaskExt as _};

pub struct RenameOrganizationModal {
    organization: Arc<Organization>,
    user_store: Entity<UserStore>,
    workspace: WeakEntity<Workspace>,
    editor: Entity<Editor>,
}

impl EventEmitter<DismissEvent> for RenameOrganizationModal {}
impl ModalView for RenameOrganizationModal {}

impl Focusable for RenameOrganizationModal {
    fn focus_handle(&self, cx: &App) -> gpui::FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl RenameOrganizationModal {
    pub fn new(
        organization: Arc<Organization>,
        user_store: Entity<UserStore>,
        workspace: WeakEntity<Workspace>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let editor = cx.new(|cx| {
            let mut editor = Editor::single_line(window, cx);
            editor.set_placeholder_text("Organization name", window, cx);
            editor.set_text(organization.name.as_ref(), window, cx);
            editor.select_all(&editor::actions::SelectAll, window, cx);
            editor
        });

        Self {
            organization,
            user_store,
            workspace,
            editor,
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        let new_name = self.editor.read(cx).text(cx);
        let task = self.user_store.update(cx, |user_store, cx| {
            user_store.rename_organization(self.organization.id.clone(), &new_name, cx)
        });
        task.detach_and_notify_err(self.workspace.clone(), window, cx);
        cx.emit(DismissEvent);
    }
}

impl Render for RenameOrganizationModal {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("RenameOrganizationModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .elevation_3(cx)
            .w_96()
            .overflow_hidden()
            .child(
                v_flex()
                    .p_2()
                    .gap_2()
                    .child(Label::new("Rename Organization"))
                    .child(self.editor.clone())
                    .child(
                        Label::new("Press enter to rename, or escape to cancel.")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
    }
}
