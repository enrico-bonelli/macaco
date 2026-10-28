//! Files changed by other programs: a bar above the editor tells when the
//! file of the active tab was modified or deleted on disk and offers to
//! reload it; saving over a modified file asks first.

use super::Editor;
use crate::doc::{DiskChange, Doc};
use gtk::prelude::*;
use gtk::{gio, glib, pango};
use std::path::PathBuf;
use std::rc::{Rc, Weak};
use std::time::Duration;

/// Other programs write a file in several steps: check after a pause.
const SETTLE: Duration = Duration::from_millis(300);

pub(super) struct DiskBar {
    pub(super) revealer: gtk::Revealer,
    label: gtk::Label,
    reload: gtk::Button,
    ignore: gtk::Button,
    close: gtk::Button,
}

pub(super) fn build_bar() -> DiskBar {
    let icon = gtk::Image::from_icon_name("dialog-warning-symbolic");
    let label = gtk::Label::new(None);
    label.set_xalign(0.0);
    label.set_hexpand(true);
    label.set_wrap(true);
    label.set_wrap_mode(pango::WrapMode::WordChar);
    let button = |text: &str, action: &str| {
        let b = gtk::Button::with_label(text);
        b.set_action_name(Some(action));
        b.set_valign(gtk::Align::Center);
        b
    };
    let reload = button("Ricarica", "win.disk-reload");
    reload.add_css_class("suggested-action");
    let ignore = button("Ignora", "win.disk-ignore");
    let close = button("Chiudi scheda", "win.close-tab");
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    b.add_css_class("disk-bar");
    for w in [icon.upcast_ref::<gtk::Widget>(), label.upcast_ref(), close.upcast_ref(), ignore.upcast_ref(), reload.upcast_ref()] {
        b.append(w);
    }
    let revealer = gtk::Revealer::builder().child(&b).transition_type(gtk::RevealerTransitionType::SlideDown).transition_duration(120).build();
    DiskBar { revealer, label, reload, ignore, close }
}

impl Editor {
    pub(super) fn setup_disk_actions(self: &Rc<Self>) {
        let a = gio::SimpleAction::new("disk-reload", None);
        let ed = self.clone();
        a.connect_activate(move |_, _| {
            if let Some(d) = ed.active_doc() {
                ed.reopen_with(&d, d.encoding.get());
            }
        });
        self.win.add_action(&a);
        let a = gio::SimpleAction::new("disk-ignore", None);
        let ed = self.clone();
        a.connect_activate(move |_, _| {
            if let Some(d) = ed.active_doc() {
                // The version on disk becomes the known one: only later
                // changes are reported.
                d.record_disk_stamp();
                ed.update_disk_bar();
            }
        });
        self.win.add_action(&a);
        // Back to the editor: files may have changed in the meantime.
        let ed = Rc::downgrade(self);
        self.win.connect_is_active_notify(move |w| {
            if let (true, Some(ed)) = (w.is_active(), ed.upgrade()) {
                ed.check_disk_all();
            }
        });
    }

    /// Watch the file of a tab.
    pub(super) fn watch_doc(self: &Rc<Self>, d: &Rc<Doc>) {
        let (ed, wd) = (Rc::downgrade(self), d.weak());
        d.set_disk_handler(Rc::new(move || {
            if let Some(ed) = ed.upgrade() {
                ed.schedule_disk_check(&wd);
            }
        }));
    }

    fn schedule_disk_check(self: &Rc<Self>, d: &Weak<Doc>) {
        let Some(doc) = d.upgrade() else { return };
        if !self.disk_pending.borrow_mut().insert(doc.id.borrow().clone()) {
            return;
        }
        let (ed, d) = (Rc::downgrade(self), d.clone());
        glib::timeout_add_local_once(SETTLE, move || {
            let (Some(ed), Some(d)) = (ed.upgrade(), d.upgrade()) else { return };
            ed.disk_pending.borrow_mut().remove(&*d.id.borrow());
            ed.check_disk_doc(&d);
        });
    }

    pub(super) fn check_disk_all(&self) {
        let docs: Vec<Rc<Doc>> = self.docs.borrow().iter().filter(|d| d.is_loaded()).cloned().collect();
        for d in docs {
            self.check_disk_doc(&d);
        }
    }

    fn check_disk_doc(&self, d: &Rc<Doc>) {
        let change = d.check_disk();
        if change == d.disk_change.get() {
            return;
        }
        d.disk_change.set(change);
        if self.is_active(d) {
            self.update_disk_bar();
        } else if change != DiskChange::None {
            let what = if change == DiskChange::Deleted { "eliminato" } else { "modificato" };
            self.message(&format!("{} è stato {what} da un altro programma", d.title()));
        }
    }

    /// Show the bar for the active tab, if its file changed.
    pub(super) fn update_disk_bar(&self) {
        let bar = &self.disk_bar;
        let Some(d) = self.active_doc() else {
            bar.revealer.set_reveal_child(false);
            return;
        };
        let name = d.title();
        let unsaved = d.is_modified();
        let text = match d.disk_change.get() {
            DiskChange::None => {
                bar.revealer.set_reveal_child(false);
                return;
            }
            DiskChange::Modified if unsaved => format!(
                "«{name}» è stato modificato da un altro programma. Hai modifiche non salvate: ricaricando le perdi, salvando sovrascrivi quelle dell'altro programma."
            ),
            DiskChange::Modified => format!("«{name}» è stato modificato da un altro programma."),
            DiskChange::Deleted => {
                format!("«{name}» è stato eliminato o spostato. Il testo resta nell'editor: salvandolo ricrei il file.")
            }
        };
        let deleted = d.disk_change.get() == DiskChange::Deleted;
        bar.label.set_text(&text);
        bar.reload.set_visible(!deleted);
        bar.close.set_visible(deleted);
        bar.ignore.set_label(if deleted { "Mantieni" } else { "Ignora" });
        bar.revealer.set_reveal_child(true);
    }

    /// Before writing `d` to `path`: if another program changed that file,
    /// ask whether to overwrite it. Calls `then(true)` to go on.
    pub(super) fn confirm_overwrite(self: &Rc<Self>, d: &Rc<Doc>, path: &PathBuf, then: Box<dyn FnOnce(bool)>) {
        let own = d.path.borrow().as_ref() == Some(path);
        if !own || d.check_disk() != DiskChange::Modified {
            then(true);
            return;
        }
        let dialog = gtk::AlertDialog::builder()
            .modal(true)
            .message(format!("«{}» è stato modificato da un altro programma", d.title()))
            .detail("Salvando sostituisci il file su disco con il testo dell'editor: le modifiche fatte dall'altro programma andranno perse.")
            .buttons(["Annulla", "Sovrascrivi"])
            .cancel_button(0)
            .default_button(0)
            .build();
        dialog.choose(Some(&self.win), None::<&gio::Cancellable>, move |res| then(res == Ok(1)));
    }
}
