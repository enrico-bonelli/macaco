//! Assistant panel: a conversation with a coding agent (Claude Code,
//! Codex) through the Agent Client Protocol. The agent reads and writes
//! files through the editor (open documents included, unsaved changes
//! too) and asks before running tools when its settings want it.

use super::Editor;
use crate::acp::{self, Agent, Responder};
use crate::{config, diff, encoding, finder};
use gtk::prelude::*;
use gtk::{gdk, glib, pango};
use serde_json::{Value, json};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::Duration;

/// Text shown for a tool output, at most.
const MAX_TOOL_TEXT: usize = 4000;

#[derive(Clone, Copy, PartialEq)]
enum Speaker {
    Agent,
    Thought,
    User,
}

/// A message being streamed: its label and text.
struct Streamed {
    who: Speaker,
    label: gtk::Label,
    text: String,
}

/// A tool call shown in the conversation.
struct ToolRow {
    status: gtk::Label,
    title: gtk::Label,
    body: gtk::Box,
}

/// A request of the agent waiting for the user (shared by its buttons).
type PendingAnswer = Rc<RefCell<Option<Responder>>>;

pub(super) struct AgentPanel {
    ed: Weak<Editor>,
    pub(super) widget: gtk::Box,
    /// The agents installed, in the order of `choice`.
    specs: Vec<&'static acp::AgentSpec>,
    choice: gtk::DropDown,
    status: gtk::Label,
    list: gtk::Box,
    scroll: gtk::ScrolledWindow,
    input: gtk::TextView,
    send: gtk::Button,
    stop: gtk::Button,
    attach: gtk::CheckButton,
    agent: RefCell<Option<Rc<Agent>>>,
    session: RefCell<Option<String>>,
    /// The agent is starting or opening a session: a prompt waits.
    starting: Cell<bool>,
    queued: RefCell<Option<Vec<Value>>>,
    busy: Cell<bool>,
    streamed: RefCell<Option<Streamed>>,
    /// The streamed label is updated shortly (chunks come in bursts).
    stream_pending: Cell<bool>,
    tools: RefCell<HashMap<String, ToolRow>>,
    plan: RefCell<Option<gtk::Box>>,
    answers: RefCell<Vec<PendingAnswer>>,
}

/// Pango markup of `md`: headings and `**bold**` in bold, code in
/// monospace, links clickable; the rest as plain text.
pub(crate) fn markdown_markup(md: &str) -> String {
    let mut out = String::with_capacity(md.len() + 64);
    let mut in_code = false;
    for line in md.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            out.push_str("<tt>");
            out.push_str(&glib::markup_escape_text(line));
            out.push_str("</tt>\n");
            continue;
        }
        let hashes = t.bytes().take_while(|&c| c == b'#').count();
        if (1..=6).contains(&hashes) && t[hashes..].starts_with(' ') {
            out.push_str("<b>");
            out.push_str(&inline_markup(t[hashes..].trim()));
            out.push_str("</b>\n");
            continue;
        }
        let indent = &line[..line.len() - t.len()];
        match t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")) {
            Some(item) => {
                out.push_str(indent);
                out.push_str("• ");
                out.push_str(&inline_markup(item));
            }
            None => out.push_str(&inline_markup(line)),
        }
        out.push('\n');
    }
    while out.ends_with('\n') {
        out.pop();
    }
    out
}

/// Markup of one line: `code`, **bold** and [links](url).
fn inline_markup(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    let mut bold = false;
    while !rest.is_empty() {
        let next = rest.find(['`', '*', '[']).unwrap_or(rest.len());
        out.push_str(&glib::markup_escape_text(&rest[..next]));
        rest = &rest[next..];
        if rest.is_empty() {
            break;
        }
        if let Some(r) = rest.strip_prefix('`')
            && let Some(end) = r.find('`')
        {
            out.push_str("<tt>");
            out.push_str(&glib::markup_escape_text(&r[..end]));
            out.push_str("</tt>");
            rest = &r[end + 1..];
        } else if let Some(r) = rest.strip_prefix("**")
            && (bold || r.contains("**"))
        {
            out.push_str(if bold { "</b>" } else { "<b>" });
            bold = !bold;
            rest = r;
        } else if let Some(r) = rest.strip_prefix('[')
            && let Some(close) = r.find("](")
            && let Some(end) = r[close + 2..].find(')')
        {
            let (text, url) = (&r[..close], &r[close + 2..close + 2 + end]);
            out.push_str(&format!("<a href=\"{}\">{}</a>", glib::markup_escape_text(url), glib::markup_escape_text(text)));
            rest = &r[close + 3 + end..];
        } else {
            let c = rest.chars().next().unwrap_or(' ');
            out.push_str(&glib::markup_escape_text(&c.to_string()));
            rest = &rest[c.len_utf8()..];
        }
    }
    if bold {
        out.push_str("</b>");
    }
    out
}

fn message_label(css: &str) -> gtk::Label {
    let l = gtk::Label::new(None);
    l.set_xalign(0.0);
    l.set_wrap(true);
    l.set_wrap_mode(pango::WrapMode::WordChar);
    l.set_selectable(true);
    l.add_css_class(css);
    l
}

/// Status of a tool call as a symbol.
fn status_mark(status: &str) -> &'static str {
    match status {
        "in_progress" => "◐",
        "completed" => "✓",
        "failed" => "✗",
        _ => "○",
    }
}

impl AgentPanel {
    fn new(ed: &Rc<Editor>, specs: Vec<&'static acp::AgentSpec>) -> Rc<AgentPanel> {
        let names: Vec<&str> = specs.iter().map(|a| a.label).collect();
        let choice = gtk::DropDown::from_strings(&names);
        let wanted = ed.shared.settings.borrow().agent.clone();
        choice.set_selected(specs.iter().position(|a| a.id == wanted).unwrap_or(0) as u32);
        choice.set_tooltip_text(Some("Agente (cambiando agente inizia una nuova conversazione)"));
        let new_chat = gtk::Button::from_icon_name("document-new-symbolic");
        new_chat.set_tooltip_text(Some("Nuova conversazione"));
        new_chat.set_has_frame(false);
        let quit = gtk::Button::from_icon_name("process-stop-symbolic");
        quit.set_tooltip_text(Some("Termina l'agente e libera la sua memoria (la conversazione finisce)"));
        quit.set_has_frame(false);
        let close = gtk::Button::from_icon_name("window-close-symbolic");
        close.set_tooltip_text(Some("Chiudi il pannello (l'agente resta attivo)"));
        close.set_has_frame(false);
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        head.add_css_class("sidebar-header");
        let title = gtk::Label::new(Some("Assistente"));
        title.add_css_class("workdir");
        head.append(&title);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        head.append(&spacer);
        head.append(&choice);
        head.append(&new_chat);
        head.append(&quit);
        head.append(&close);

        let status = gtk::Label::new(None);
        status.set_xalign(0.0);
        status.set_wrap(true);
        status.add_css_class("dim-label");
        status.set_margin_start(8);
        status.set_margin_end(8);

        let list = gtk::Box::new(gtk::Orientation::Vertical, 10);
        list.set_margin_start(8);
        list.set_margin_end(8);
        list.set_margin_top(8);
        list.set_margin_bottom(8);
        let scroll = gtk::ScrolledWindow::builder().child(&list).hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).build();

        let input = gtk::TextView::new();
        input.set_wrap_mode(gtk::WrapMode::WordChar);
        input.set_accepts_tab(false);
        input.set_top_margin(6);
        input.set_bottom_margin(6);
        input.set_left_margin(6);
        input.set_right_margin(6);
        input.add_css_class("agent-input");
        let input_scroll = gtk::ScrolledWindow::builder()
            .child(&input)
            .hscrollbar_policy(gtk::PolicyType::Never)
            .min_content_height(70)
            .max_content_height(220)
            .propagate_natural_height(true)
            .build();
        input_scroll.add_css_class("frame");
        let attach = gtk::CheckButton::with_label("File corrente");
        attach.set_active(true);
        attach.set_tooltip_text(Some("Indica all'agente il file attivo e il testo selezionato"));
        let stop = gtk::Button::with_label("Interrompi");
        stop.set_sensitive(false);
        let send = gtk::Button::with_label("Invia");
        send.add_css_class("suggested-action");
        send.set_tooltip_text(Some("Invio: invia · Maiusc+Invio: a capo"));
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        actions.append(&attach);
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        actions.append(&spacer);
        actions.append(&stop);
        actions.append(&send);
        let bottom = gtk::Box::new(gtk::Orientation::Vertical, 6);
        bottom.set_margin_start(8);
        bottom.set_margin_end(8);
        bottom.set_margin_bottom(8);
        bottom.append(&input_scroll);
        bottom.append(&actions);

        let widget = gtk::Box::new(gtk::Orientation::Vertical, 4);
        widget.add_css_class("agent-panel");
        widget.append(&head);
        widget.append(&status);
        widget.append(&scroll);
        widget.append(&bottom);

        let p = Rc::new(AgentPanel {
            ed: Rc::downgrade(ed),
            widget,
            specs,
            choice,
            status,
            list,
            scroll,
            input,
            send,
            stop,
            attach,
            agent: RefCell::new(None),
            session: RefCell::new(None),
            starting: Cell::new(false),
            queued: RefCell::new(None),
            busy: Cell::new(false),
            streamed: RefCell::new(None),
            stream_pending: Cell::new(false),
            tools: RefCell::new(HashMap::new()),
            plan: RefCell::new(None),
            answers: RefCell::new(Vec::new()),
        });
        let w = Rc::downgrade(&p);
        p.send.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.send_prompt();
            }
        });
        let w = Rc::downgrade(&p);
        p.stop.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.cancel();
            }
        });
        let w = Rc::downgrade(&p);
        new_chat.connect_clicked(move |_| {
            if let Some(p) = w.upgrade() {
                p.new_conversation();
            }
        });
        let w = Rc::downgrade(&p);
        quit.connect_clicked(move |_| {
            let Some(p) = w.upgrade() else { return };
            if p.agent.borrow().is_none() {
                return;
            }
            p.stop_agent();
            p.notice(&format!("{} terminato", p.spec().label), false);
            p.set_status(&p.intro());
            if let Some(ed) = p.ed.upgrade() {
                ed.release_memory();
            }
        });
        let w = Rc::downgrade(&p);
        close.connect_clicked(move |_| {
            if let Some(ed) = w.upgrade().and_then(|p| p.ed.upgrade()) {
                ed.set_assistant(false);
            }
        });
        let w = Rc::downgrade(&p);
        p.choice.connect_selected_notify(move |c| {
            let Some(p) = w.upgrade() else { return };
            let Some(spec) = p.specs.get(c.selected() as usize).copied() else { return };
            if let Some(ed) = p.ed.upgrade() {
                ed.shared.settings.borrow_mut().agent = spec.id.to_string();
                ed.update_agent_buttons();
            }
            // Another agent: another conversation.
            p.stop_agent();
            p.clear();
            p.set_status(&format!("{}: scrivi un messaggio per iniziare", spec.label));
        });
        // Enter sends, Shift+Enter starts a new line.
        let keys = gtk::EventControllerKey::new();
        let w = Rc::downgrade(&p);
        keys.connect_key_pressed(move |_, key, _, state| {
            let enter = matches!(key, gdk::Key::Return | gdk::Key::KP_Enter);
            if enter && !state.contains(gdk::ModifierType::SHIFT_MASK) {
                if let Some(p) = w.upgrade() {
                    p.send_prompt();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        p.input.add_controller(keys);
        p.set_status(&p.intro());
        p
    }

    pub(super) fn spec(&self) -> &'static acp::AgentSpec {
        self.specs.get(self.choice.selected() as usize).copied().unwrap_or(&acp::AGENTS[0])
    }

    /// Status line when nothing happened yet.
    fn intro(&self) -> String {
        let spec = self.spec();
        if acp::find_command(spec).is_some() {
            format!("{}: scrivi un messaggio per iniziare", spec.label)
        } else {
            format!("{} richiede il suo adattatore ACP: npm install -g {}", spec.label, spec.package)
        }
    }

    fn set_status(&self, text: &str) {
        self.status.set_text(text);
        self.status.set_visible(!text.is_empty());
    }

    fn update_buttons(&self) {
        self.stop.set_sensitive(self.busy.get());
        self.send.set_sensitive(!self.busy.get());
    }

    pub(super) fn focus(&self) {
        self.input.grab_focus();
    }

    // ---------------------------------------------------------- the agent

    /// The folder the agent works in: the working folder, else the folder
    /// of the active file.
    fn cwd(&self) -> PathBuf {
        let Some(ed) = self.ed.upgrade() else { return PathBuf::from("/") };
        ed.workdir.borrow().clone().unwrap_or_else(|| ed.doc_dir())
    }

    /// The buttons of the status bar show which agent runs.
    fn buttons_changed(&self) {
        if let Some(ed) = self.ed.upgrade() {
            ed.update_agent_buttons();
        }
    }

    /// The agent the panel uses, if installed.
    pub(super) fn select(&self, id: &str) {
        if let Some(i) = self.specs.iter().position(|a| a.id == id)
            && self.choice.selected() != i as u32
        {
            // Another agent: `connect_selected_notify` starts over.
            self.choice.set_selected(i as u32);
        }
    }

    pub(super) fn running(&self) -> Option<&'static str> {
        self.agent.borrow().as_ref().map(|a| a.spec.id)
    }

    fn stop_agent(&self) {
        if let Some(a) = self.agent.borrow_mut().take() {
            a.stop();
        }
        self.buttons_changed();
        *self.session.borrow_mut() = None;
        *self.queued.borrow_mut() = None;
        self.starting.set(false);
        self.busy.set(false);
        self.cancel_answers();
        self.update_buttons();
    }

    fn clear(&self) {
        self.finish_stream();
        while let Some(c) = self.list.first_child() {
            self.list.remove(&c);
        }
        self.tools.borrow_mut().clear();
        *self.plan.borrow_mut() = None;
    }

    fn new_conversation(self: &Rc<Self>) {
        if self.busy.get() {
            self.cancel();
        }
        self.clear();
        *self.session.borrow_mut() = None;
        if self.agent.borrow().is_some() {
            self.open_session();
        } else {
            self.set_status(&self.intro());
        }
    }

    /// Starts the agent (if needed) and a session; the queued prompt is
    /// sent when it is ready.
    fn ensure_session(self: &Rc<Self>) {
        if self.starting.get() {
            return;
        }
        if self.agent.borrow().is_some() {
            self.open_session();
            return;
        }
        let spec = self.spec();
        let w = Rc::downgrade(self);
        let w2 = w.clone();
        let w3 = w.clone();
        let handlers = acp::Handlers {
            update: Box::new(move |u| {
                if let Some(p) = w.upgrade() {
                    p.on_update(u);
                }
            }),
            request: Box::new(move |m, params, r| match w2.upgrade() {
                Some(p) => p.on_request(m, params, r),
                None => r.err(-32603, "editor chiuso"),
            }),
            exited: Box::new(move |why| {
                if let Some(p) = w3.upgrade() {
                    p.on_exit(&why);
                }
            }),
        };
        self.starting.set(true);
        self.set_status(&format!("Avvio di {}…", spec.label));
        let w = Rc::downgrade(self);
        let started = Agent::start(spec, &self.cwd(), handlers, move |res| {
            let Some(p) = w.upgrade() else { return };
            p.starting.set(false);
            match res {
                Ok(()) => p.open_session(),
                Err(e) => {
                    p.stop_agent();
                    p.fail(&e);
                }
            }
        });
        match started {
            Ok(a) => {
                *self.agent.borrow_mut() = Some(a);
                self.buttons_changed();
            }
            Err(e) => {
                self.starting.set(false);
                self.fail(&e);
            }
        }
    }

    fn open_session(self: &Rc<Self>) {
        let Some(agent) = self.agent.borrow().clone() else { return };
        self.starting.set(true);
        let cwd = self.cwd();
        let folder = cwd.file_name().map_or_else(|| cwd.display().to_string(), |n| n.to_string_lossy().into_owned());
        self.set_status(&format!("{} · {folder}", agent.spec.label));
        self.status.set_tooltip_text(Some(&format!("Cartella dell'agente: {}", config::tilde(&cwd))));
        let w = Rc::downgrade(self);
        agent.request("session/new", json!({"cwd": cwd, "mcpServers": []}), move |res| {
            let Some(p) = w.upgrade() else { return };
            p.starting.set(false);
            match res {
                Ok(v) => {
                    *p.session.borrow_mut() = v.get("sessionId").and_then(Value::as_str).map(str::to_string);
                    if let Some(blocks) = p.queued.borrow_mut().take() {
                        p.prompt(blocks);
                    }
                }
                Err(e) if e.code == acp::AUTH_REQUIRED || e.message.to_lowercase().contains("auth") => p.ask_login(&e.message),
                Err(e) => p.fail(&format!("Impossibile aprire una conversazione: {}", e.message)),
            }
        });
    }

    /// The message waiting for the agent cannot be sent.
    fn fail(&self, why: &str) {
        *self.queued.borrow_mut() = None;
        self.busy.set(false);
        self.update_buttons();
        self.notice(why, true);
        self.set_status("");
    }

    fn on_exit(self: &Rc<Self>, why: &str) {
        self.finish_stream();
        *self.agent.borrow_mut() = None;
        self.buttons_changed();
        *self.session.borrow_mut() = None;
        self.starting.set(false);
        self.busy.set(false);
        self.cancel_answers();
        self.update_buttons();
        if !why.is_empty() {
            self.notice(why, true);
        }
        self.set_status(&self.intro());
        if let Some(ed) = self.ed.upgrade() {
            ed.release_memory();
        }
    }

    /// The agent needs a login: its own methods, or its login command in
    /// the terminal.
    fn ask_login(self: &Rc<Self>, why: &str) {
        let Some(agent) = self.agent.borrow().clone() else { return };
        let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
        b.add_css_class("agent-tool");
        let text = message_label("agent-notice");
        text.set_text(&format!("{} chiede di accedere ({why}).", agent.spec.label));
        b.append(&text);
        let row = gtk::FlowBox::new();
        row.set_selection_mode(gtk::SelectionMode::None);
        // Methods run by the agent itself (the terminal ones are replaced by
        // the login command below).
        for (id, name) in agent.auth_methods() {
            let btn = gtk::Button::with_label(&name);
            let w = Rc::downgrade(self);
            btn.connect_clicked(move |_| {
                let Some(p) = w.upgrade() else { return };
                let Some(a) = p.agent.borrow().clone() else { return };
                let w = Rc::downgrade(&p);
                a.request("authenticate", json!({"methodId": id}), move |res| {
                    let Some(p) = w.upgrade() else { return };
                    match res {
                        Ok(_) => p.open_session(),
                        Err(e) => p.notice(&format!("Accesso non riuscito: {}", e.message), true),
                    }
                });
            });
            row.insert(&btn, -1);
        }
        let login = gtk::Button::with_label(&format!("Accedi nel terminale ({})", agent.spec.login));
        let (w, cmd) = (Rc::downgrade(self), agent.spec.login);
        login.connect_clicked(move |_| {
            if let Some(p) = w.upgrade()
                && let Some(ed) = p.ed.upgrade()
            {
                ed.run_in_terminal(&p.cwd(), cmd);
                p.set_status("Dopo l'accesso, invia di nuovo il messaggio");
            }
        });
        row.insert(&login, -1);
        b.append(&row);
        self.append(&b);
    }

    // ------------------------------------------------------------ prompts

    fn send_prompt(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        let buf = self.input.buffer();
        let text = buf.text(&buf.start_iter(), &buf.end_iter(), false).trim().to_string();
        if text.is_empty() {
            return;
        }
        buf.set_text("");
        let mut blocks = vec![json!({"type": "text", "text": text})];
        let mut shown = text.clone();
        if self.attach.is_active()
            && let Some((context, note)) = self.context()
        {
            blocks.extend(context);
            shown.push_str(&format!("\n\n📎 {note}"));
        }
        self.finish_stream();
        let label = message_label("agent-user");
        label.set_text(&shown);
        self.append(&label);
        if self.session.borrow().is_some() {
            self.prompt(blocks);
        } else {
            *self.queued.borrow_mut() = Some(blocks);
            self.busy.set(true);
            self.update_buttons();
            self.ensure_session();
        }
    }

    /// The active file (and the selection) for the prompt, with a short
    /// description.
    fn context(&self) -> Option<(Vec<Value>, String)> {
        let ed = self.ed.upgrade()?;
        let d = ed.active_doc()?;
        let path = d.path.borrow().clone()?;
        let uri = crate::lsp::path_to_uri(&path);
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let mut blocks = vec![json!({"type": "resource_link", "uri": uri, "name": name})];
        let mut note = name.clone();
        if let Some((a, b)) = d.buffer.selection_bounds() {
            let sel = d.buffer.text(&a, &b, true);
            let (l1, l2) = (a.line() + 1, b.line() + 1);
            note = format!("{name}:{l1}-{l2}");
            blocks.push(json!({"type": "text", "text": format!("Selezione in {} (righe {l1}-{l2}):\n```\n{sel}\n```", path.display())}));
        }
        Some((blocks, note))
    }

    fn prompt(self: &Rc<Self>, blocks: Vec<Value>) {
        let (Some(agent), Some(session)) = (self.agent.borrow().clone(), self.session.borrow().clone()) else { return };
        self.busy.set(true);
        self.update_buttons();
        self.set_status(&format!("{} sta lavorando…", agent.spec.label));
        let w = Rc::downgrade(self);
        agent.request("session/prompt", json!({"sessionId": session, "prompt": blocks}), move |res| {
            let Some(p) = w.upgrade() else { return };
            p.finish_stream();
            p.busy.set(false);
            p.cancel_answers();
            p.update_buttons();
            let label = p.spec().label;
            match res {
                Ok(v) => {
                    let reason = v.get("stopReason").and_then(Value::as_str).unwrap_or("end_turn");
                    let why = match reason {
                        "cancelled" => Some("Interrotto"),
                        "max_tokens" => Some("Interrotto: raggiunto il limite di token"),
                        "max_turn_requests" => Some("Interrotto: troppe richieste al modello in questo turno"),
                        "refusal" => Some("L'agente ha rifiutato di continuare"),
                        _ => None,
                    };
                    if let Some(why) = why {
                        p.notice(why, reason == "refusal");
                    }
                    p.set_status(&format!("{label}: pronto"));
                }
                Err(e) => {
                    p.notice(&e.message, true);
                    p.set_status("");
                }
            }
            if let Some(ed) = p.ed.upgrade() {
                // Files changed by the agent: git and the file panel.
                ed.git_refresh();
            }
        });
    }

    fn cancel(&self) {
        if let (Some(agent), Some(session)) = (self.agent.borrow().clone(), self.session.borrow().clone()) {
            agent.notify("session/cancel", json!({"sessionId": session}));
        } else {
            // Still starting: nothing to cancel on the agent side.
            *self.queued.borrow_mut() = None;
            self.busy.set(false);
            self.update_buttons();
        }
        self.cancel_answers();
    }

    /// Questions still waiting are answered "cancelled".
    fn cancel_answers(&self) {
        for a in self.answers.borrow_mut().drain(..) {
            if let Some(r) = a.borrow_mut().take() {
                r.ok(json!({"outcome": {"outcome": "cancelled"}}));
            }
        }
    }

    // --------------------------------------------------------- the output

    /// Adds `w` at the end, keeping the end in view if it was.
    fn append(&self, w: &impl IsA<gtk::Widget>) {
        let adj = self.scroll.vadjustment();
        let at_end = adj.value() >= adj.upper() - adj.page_size() - 40.0;
        self.list.append(w);
        if at_end {
            self.scroll_to_end();
        }
    }

    fn scroll_to_end(&self) {
        let adj = self.scroll.vadjustment();
        glib::idle_add_local_once(move || adj.set_value(adj.upper() - adj.page_size()));
    }

    /// A line of text from the editor (errors in red).
    fn notice(&self, text: &str, error: bool) {
        self.finish_stream();
        let l = message_label("agent-notice");
        if error {
            l.add_css_class("error");
        }
        l.set_text(text);
        self.append(&l);
    }

    /// The message being streamed is complete: shown with its formatting.
    fn finish_stream(&self) {
        let Some(s) = self.streamed.borrow_mut().take() else { return };
        match s.who {
            Speaker::Agent => s.label.set_markup(&markdown_markup(&s.text)),
            _ => s.label.set_text(s.text.trim_end()),
        }
    }

    fn append_chunk(self: &Rc<Self>, who: Speaker, text: &str) {
        let same = self.streamed.borrow().as_ref().is_some_and(|s| s.who == who);
        if !same {
            self.finish_stream();
            let label = message_label(match who {
                Speaker::Agent => "agent-message",
                Speaker::Thought => "agent-thought",
                Speaker::User => "agent-user",
            });
            self.append(&label);
            *self.streamed.borrow_mut() = Some(Streamed { who, label, text: String::new() });
        }
        if let Some(s) = self.streamed.borrow_mut().as_mut() {
            s.text.push_str(text);
        }
        // Shown a few times a second while it grows.
        if !self.stream_pending.replace(true) {
            let w = Rc::downgrade(self);
            glib::timeout_add_local_once(Duration::from_millis(60), move || {
                let Some(p) = w.upgrade() else { return };
                p.stream_pending.set(false);
                let adj = p.scroll.vadjustment();
                let at_end = adj.value() >= adj.upper() - adj.page_size() - 40.0;
                if let Some(s) = p.streamed.borrow().as_ref() {
                    s.label.set_text(&s.text);
                }
                if at_end {
                    p.scroll_to_end();
                }
            });
        }
    }

    fn on_update(self: &Rc<Self>, u: &Value) {
        let kind = u.get("sessionUpdate").and_then(Value::as_str).unwrap_or("");
        let content_text = || u.get("content").and_then(acp::block_text).unwrap_or("").to_string();
        match kind {
            "agent_message_chunk" => self.append_chunk(Speaker::Agent, &content_text()),
            "agent_thought_chunk" => self.append_chunk(Speaker::Thought, &content_text()),
            "user_message_chunk" => self.append_chunk(Speaker::User, &content_text()),
            "tool_call" | "tool_call_update" => self.tool_update(u),
            "plan" => self.show_plan(u),
            _ => {}
        }
    }

    fn tool_update(self: &Rc<Self>, u: &Value) {
        let Some(id) = u.get("toolCallId").and_then(Value::as_str) else { return };
        if !self.tools.borrow().contains_key(id) {
            self.finish_stream();
            let status = gtk::Label::new(Some(status_mark("pending")));
            let title = gtk::Label::new(None);
            title.set_xalign(0.0);
            title.set_wrap(true);
            title.set_wrap_mode(pango::WrapMode::WordChar);
            title.set_hexpand(true);
            let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            head.append(&status);
            head.append(&title);
            let body = gtk::Box::new(gtk::Orientation::Vertical, 4);
            let b = gtk::Box::new(gtk::Orientation::Vertical, 4);
            b.add_css_class("agent-tool");
            b.append(&head);
            b.append(&body);
            self.append(&b);
            self.tools.borrow_mut().insert(id.to_string(), ToolRow { status, title, body });
        }
        let tools = self.tools.borrow();
        let row = &tools[id];
        if let Some(t) = u.get("title").and_then(Value::as_str) {
            row.title.set_text(t);
        }
        if let Some(s) = u.get("status").and_then(Value::as_str) {
            row.status.set_text(status_mark(s));
            row.status.set_tooltip_text(Some(s));
        }
        // The content replaces the previous one.
        if let Some(items) = u.get("content").and_then(Value::as_array) {
            while let Some(c) = row.body.first_child() {
                row.body.remove(&c);
            }
            for item in items {
                match item.get("type").and_then(Value::as_str) {
                    Some("diff") => row.body.append(&self.diff_widget(item)),
                    Some("content") => {
                        let text = item.get("content").and_then(acp::block_text).unwrap_or("");
                        if !text.trim().is_empty() {
                            row.body.append(&tool_text(text));
                        }
                    }
                    _ => {}
                }
            }
        }
        if let Some(locs) = u.get("locations").and_then(Value::as_array)
            && !locs.is_empty()
            && row.body.first_child().is_none()
        {
            for l in locs.iter().take(5) {
                let Some(path) = l.get("path").and_then(Value::as_str) else { continue };
                let line = l.get("line").and_then(Value::as_u64);
                row.body.append(&self.open_button(Path::new(path), line));
            }
        }
    }

    /// A button opening `path` (at 1-based `line`).
    fn open_button(&self, path: &Path, line: Option<u64>) -> gtk::Button {
        let name = match line {
            Some(l) => format!("{}:{l}", self.rel(path)),
            None => self.rel(path),
        };
        let b = gtk::Button::with_label(&name);
        b.set_has_frame(false);
        b.set_halign(gtk::Align::Start);
        b.add_css_class("agent-link");
        let (ed, path) = (self.ed.clone(), path.to_path_buf());
        b.connect_clicked(move |_| {
            if let Some(ed) = ed.upgrade() {
                ed.open_at(&path, line.map_or(0, |l| l.saturating_sub(1)) as u32, 0);
            }
        });
        b
    }

    fn rel(&self, path: &Path) -> String {
        finder::rel_or_full(&self.cwd(), path)
    }

    /// The changes of an edit, as a colored unified diff.
    fn diff_widget(&self, item: &Value) -> gtk::Widget {
        let path = item.get("path").and_then(Value::as_str).unwrap_or("");
        let old = item.get("oldText").and_then(Value::as_str).unwrap_or("");
        let new = item.get("newText").and_then(Value::as_str).unwrap_or("");
        let rel = self.rel(Path::new(path));
        let unified = diff::unified(old, new, &rel, &rel);
        let (mut add, mut del) = (0, 0);
        for l in unified.lines().filter(|l| !l.starts_with("+++") && !l.starts_with("---")) {
            add += usize::from(l.starts_with('+'));
            del += usize::from(l.starts_with('-'));
        }
        let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
        let head = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        head.append(&self.open_button(Path::new(path), None));
        let stats = gtk::Label::new(Some(&format!("+{add} −{del}")));
        stats.add_css_class("dim-label");
        head.append(&stats);
        b.append(&head);
        let Some(ed) = self.ed.upgrade() else { return b.upcast() };
        let view = super::history::diff_view(&ed.shared.tags.table);
        super::history::show_diff(&view, unified.lines().skip(2).collect::<Vec<_>>().join("\n").as_str());
        let expander = gtk::Expander::new(Some("Modifiche"));
        expander.set_child(Some(&view));
        expander.set_expanded(add + del <= 40);
        b.append(&expander);
        b.upcast()
    }

    fn show_plan(&self, u: &Value) {
        let Some(entries) = u.get("entries").and_then(Value::as_array) else { return };
        let fresh = self.plan.borrow().is_none();
        if fresh {
            self.finish_stream();
            let b = gtk::Box::new(gtk::Orientation::Vertical, 2);
            b.add_css_class("agent-tool");
            self.append(&b);
            *self.plan.borrow_mut() = Some(b);
        }
        let plan = self.plan.borrow();
        let Some(b) = plan.as_ref() else { return };
        while let Some(c) = b.first_child() {
            b.remove(&c);
        }
        let title = gtk::Label::new(Some("Piano"));
        title.set_xalign(0.0);
        title.add_css_class("problems-title");
        b.append(&title);
        for e in entries {
            let text = e.get("content").and_then(Value::as_str).unwrap_or("");
            let mark = match e.get("status").and_then(Value::as_str) {
                Some("completed") => "☑",
                Some("in_progress") => "◐",
                _ => "☐",
            };
            let l = gtk::Label::new(Some(&format!("{mark} {text}")));
            l.set_xalign(0.0);
            l.set_wrap(true);
            b.append(&l);
        }
    }

    // ----------------------------------------------- requests of the agent

    fn on_request(self: &Rc<Self>, method: &str, params: &Value, r: Responder) {
        match method {
            "session/request_permission" => self.ask_permission(params, r),
            "fs/read_text_file" => {
                let path = params.get("path").and_then(Value::as_str).map(PathBuf::from);
                let line = params.get("line").and_then(Value::as_u64);
                let limit = params.get("limit").and_then(Value::as_u64);
                match path.and_then(|p| self.read_file(&p)) {
                    Some(text) => r.ok(json!({"content": acp::slice_lines(&text, line, limit)})),
                    None => r.err(-32002, "file non trovato"),
                }
            }
            "fs/write_text_file" => {
                let path = params.get("path").and_then(Value::as_str).map(PathBuf::from);
                let content = params.get("content").and_then(Value::as_str).unwrap_or("");
                match path.map(|p| self.write_file(&p, content)) {
                    Some(Ok(())) => r.ok(json!({})),
                    Some(Err(e)) => r.err(-32603, &e),
                    None => r.err(-32602, "percorso mancante"),
                }
            }
            _ => r.err(-32601, "metodo non supportato da macaco"),
        }
    }

    /// The text of `path`: from the editor when open (unsaved changes
    /// included), else from the disk.
    fn read_file(&self, path: &Path) -> Option<String> {
        let ed = self.ed.upgrade()?;
        let doc = ed.docs.borrow().iter().find(|d| d.path.borrow().as_deref() == Some(path) && d.is_loaded()).cloned();
        match doc {
            Some(d) => Some(d.text()),
            None => finder::read_text(path),
        }
    }

    /// Writes `content` to `path`: through the editor when open (one undo
    /// step, then saved), else to the disk keeping its encoding and line
    /// endings.
    fn write_file(&self, path: &Path, content: &str) -> Result<(), String> {
        let ed = self.ed.upgrade().ok_or("editor chiuso")?;
        let doc = ed.docs.borrow().iter().find(|d| d.path.borrow().as_deref() == Some(path) && d.is_loaded()).cloned();
        if let Some(d) = doc {
            if d.text() != content {
                d.replace_all_text(content);
            }
            return if ed.write_doc(&d, path) { Ok(()) } else { Err(format!("impossibile salvare {}", path.display())) };
        }
        let (enc, eol) = match crate::doc::read_file(path, None) {
            Ok((_, enc, eol)) => (enc, eol),
            Err(_) => (encoding::Encoding::Utf8, encoding::Eol::Lf),
        };
        let (bytes, _) = encoding::encode(&encoding::apply_eol(content, eol), enc);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        // Written in place: the permissions (executable scripts) stay.
        std::fs::write(path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        ed.sidebar_refresh();
        Ok(())
    }

    /// The agent asks before running a tool: one button per option.
    fn ask_permission(self: &Rc<Self>, params: &Value, r: Responder) {
        self.finish_stream();
        let answer: PendingAnswer = Rc::new(RefCell::new(Some(r)));
        self.answers.borrow_mut().push(answer.clone());
        let title = params
            .pointer("/toolCall/title")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| {
                let id = params.pointer("/toolCall/toolCallId").and_then(Value::as_str)?;
                self.tools.borrow().get(id).map(|t| t.title.text().to_string())
            })
            .unwrap_or_else(|| "un'operazione".to_string());
        let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
        b.add_css_class("agent-tool");
        b.add_css_class("agent-permission");
        let q = message_label("agent-notice");
        q.set_text(&format!("Permetti: {title}?"));
        b.append(&q);
        let row = gtk::FlowBox::new();
        row.set_selection_mode(gtk::SelectionMode::None);
        row.set_max_children_per_line(4);
        let options = params.get("options").and_then(Value::as_array).cloned().unwrap_or_default();
        for o in options {
            let (Some(id), Some(name)) = (o.get("optionId").and_then(Value::as_str), o.get("name").and_then(Value::as_str)) else { continue };
            let btn = gtk::Button::with_label(name);
            if o.get("kind").and_then(Value::as_str).is_some_and(|k| k.starts_with("allow")) {
                btn.add_css_class("suggested-action");
            }
            let (answer, id, name, q, row_w) = (answer.clone(), id.to_string(), name.to_string(), q.clone(), row.downgrade());
            btn.connect_clicked(move |_| {
                if let Some(r) = answer.borrow_mut().take() {
                    r.ok(json!({"outcome": {"outcome": "selected", "optionId": id}}));
                    q.set_text(&format!("{} → {name}", q.text()));
                    if let Some(row) = row_w.upgrade() {
                        row.set_visible(false);
                    }
                }
            });
            row.insert(&btn, -1);
        }
        b.append(&row);
        self.append(&b);
        self.set_status("L'agente attende una tua risposta");
    }

    /// Memory (kB) of the agent and its children.
    pub(super) fn memory_kb(&self) -> u64 {
        let Some(pid) = self.agent.borrow().as_ref().and_then(|a| a.pid()) else { return 0 };
        let mut total = 0;
        let mut stack = vec![pid];
        while let Some(p) = stack.pop() {
            total += crate::proc::rss_kb(&p);
            if let Ok(children) = std::fs::read_to_string(format!("/proc/{p}/task/{p}/children")) {
                stack.extend(children.split_whitespace().map(str::to_string));
            }
        }
        total
    }

    pub(super) fn shutdown(&self) {
        if let Some(a) = self.agent.borrow_mut().take() {
            a.stop();
        }
    }
}

#[cfg(debug_assertions)]
impl AgentPanel {
    /// Starts the agent and a session, without a message.
    pub(super) fn debug_start(self: &Rc<Self>) {
        self.ensure_session();
    }

    pub(super) fn debug_info(&self) {
        let info = self.agent.borrow().as_ref().map(|a| a.info.borrow().clone());
        eprintln!("AGENT session={:?} info={}", self.session.borrow(), info.map(|i| i.to_string()).unwrap_or_default());
    }

    pub(super) fn debug_send(self: &Rc<Self>, text: &str) {
        self.input.buffer().set_text(text);
        self.send_prompt();
    }

    /// Clicks button `n` of the last question of the agent.
    pub(super) fn debug_answer(&self, n: usize) {
        let mut w = self.list.last_child();
        while let Some(c) = w {
            if c.has_css_class("agent-permission") {
                let flow = c.last_child().and_downcast::<gtk::FlowBox>();
                if let Some(child) = flow.and_then(|f| f.child_at_index(n as i32))
                    && let Some(b) = child.child().and_downcast::<gtk::Button>()
                {
                    b.emit_clicked();
                }
                return;
            }
            w = c.prev_sibling();
        }
    }
}

/// Output of a tool, in monospace (cut when long).
fn tool_text(text: &str) -> gtk::Widget {
    let mut t = text.trim_end().to_string();
    if t.len() > MAX_TOOL_TEXT {
        let mut cut = MAX_TOOL_TEXT;
        while !t.is_char_boundary(cut) {
            cut -= 1;
        }
        t.truncate(cut);
        t.push_str("\n…");
    }
    let l = gtk::Label::new(Some(&t));
    l.set_xalign(0.0);
    l.set_wrap(true);
    l.set_wrap_mode(pango::WrapMode::WordChar);
    l.set_selectable(true);
    l.add_css_class("monospace");
    let expander = gtk::Expander::new(Some("Risultato"));
    expander.set_child(Some(&l));
    expander.upcast()
}

impl Editor {
    pub(super) fn setup_assistant_actions(self: &Rc<Self>) {
        self.add_toggle("assistant", &["<Control><Shift>a", "F6"], |ed| ed.set_assistant(!ed.assistant_visible()));
        // The buttons of the status bar: the panel with that agent.
        self.add_str_action("assistant-agent", |ed, id| ed.toggle_agent(&id));
        if let Some(a) = self.action("assistant") {
            a.set_enabled(!self.status.agents.is_empty());
        }
    }

    /// Button of agent `id`: shows the panel with that agent, or hides it
    /// when it already shows it.
    fn toggle_agent(self: &Rc<Self>, id: &str) {
        let shown = self.assistant.borrow().as_ref().filter(|_| self.assistant_visible()).map(|p| p.spec().id);
        if shown == Some(id) {
            self.set_assistant(false);
            return;
        }
        self.shared.settings.borrow_mut().agent = id.to_string();
        self.set_assistant(true);
        if let Some(p) = self.assistant.borrow().clone() {
            p.select(id);
        }
        self.update_agent_buttons();
    }

    /// Marks the button of the agent shown and of the one running.
    pub(super) fn update_agent_buttons(&self) {
        let panel = self.assistant.borrow();
        let shown = panel.as_ref().filter(|_| self.assistant_visible()).map(|p| p.spec().id);
        let running = panel.as_ref().and_then(|p| p.running());
        for (a, b) in &self.status.agents {
            for (class, on) in [("agent-shown", shown == Some(a.id)), ("agent-running", running == Some(a.id))] {
                if on {
                    b.add_css_class(class);
                } else {
                    b.remove_css_class(class);
                }
            }
        }
    }

    pub(super) fn assistant_visible(&self) -> bool {
        self.agent_paned.end_child().is_some_and(|c| c.is_visible())
    }

    /// Shows or hides the assistant panel (created at first use; the agent
    /// keeps running while it is hidden).
    pub(super) fn set_assistant(self: &Rc<Self>, on: bool) {
        let specs: Vec<&'static acp::AgentSpec> = self.status.agents.iter().map(|(a, _)| *a).collect();
        if on && specs.is_empty() {
            self.message("Per l'assistente installa claude-agent-acp o codex-acp (vedi il README)");
            self.set_action_state("assistant", false);
            return;
        }
        if on && self.assistant.borrow().is_none() {
            let p = AgentPanel::new(self, specs);
            self.agent_paned.set_end_child(Some(&p.widget));
            *self.assistant.borrow_mut() = Some(p);
        }
        let panel = self.assistant.borrow().clone();
        if let Some(p) = &panel {
            if on && !p.widget.is_visible() {
                p.widget.set_visible(true);
                let w = self.agent_paned.width().max(self.win.default_width());
                let width = self.shared.settings.borrow().agent_width;
                self.agent_paned.set_position((w - width).max(200));
            } else if !on && p.widget.is_visible() {
                self.shared.settings.borrow_mut().agent_width = self.agent_paned.width() - self.agent_paned.position();
                p.widget.set_visible(false);
            }
            if on {
                p.focus();
            }
        }
        self.set_action_state("assistant", on);
        self.update_agent_buttons();
    }

    /// Memory (kB) of the running agent, if any.
    pub(super) fn assistant_memory_kb(&self) -> u64 {
        self.assistant.borrow().as_ref().map_or(0, |p| p.memory_kb())
    }

    pub(super) fn shutdown_assistant(&self) {
        if let Some(p) = self.assistant.borrow().as_ref() {
            p.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::markdown_markup;

    #[test]
    fn markdown() {
        assert_eq!(markdown_markup("# Titolo\n- uno `a<b>`\n**forte** e [link](http://x?a=1&b=2)"),
            "<b>Titolo</b>\n• uno <tt>a&lt;b&gt;</tt>\n<b>forte</b> e <a href=\"http://x?a=1&amp;b=2\">link</a>");
        assert_eq!(markdown_markup("```rust\nfn x() {}\n```\nfine"), "<tt>fn x() {}</tt>\nfine");
        // Unbalanced markers stay as text.
        assert_eq!(markdown_markup("a * b ** c ` d [e"), "a * b ** c ` d [e");
    }
}
