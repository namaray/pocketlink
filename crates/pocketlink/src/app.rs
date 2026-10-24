// SPDX-License-Identifier: GPL-3.0-or-later

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use cosmic::app::context_drawer;
use cosmic::iced::alignment::Horizontal;
use cosmic::iced::futures::{SinkExt, StreamExt};
use cosmic::iced::widget::stack;
use cosmic::iced::{Alignment, Background, Border, Color, Length, Subscription};
use cosmic::prelude::*;
use cosmic::widget::{self, about::About, icon, nav_bar, settings, toaster};
use pocketlink_core::{
    Client, Device, DeviceKind, Direction, Event, Notification, PairState, Transfer, TransferState,
    Transport, caps,
};

use crate::drop::{self, Payload};

const APP_ICON: &[u8] = include_bytes!("../resources/icon.svg");

/// Above this, warn that Bluetooth (roughly 100–200 KB/s) will take a while.
const BLUETOOTH_SLOW_BYTES: u64 = 10_000_000;

pub struct App {
    core: cosmic::Core,
    about: About,
    nav: nav_bar::Model,
    toasts: toaster::Toasts<Message>,
    client: Option<Client>,
    status: Status,
    devices: Vec<Device>,
    /// By id, so the newest is last.
    transfers: BTreeMap<u64, Transfer>,
    /// Texts and links we sent this session, newest first.
    sent_texts: Vec<String>,
    drag_hover: bool,
    refreshing: bool,
    refresh_again: bool,
    /// The notification being replied to, and the reply so far.
    reply: Option<(String, String)>,
}

enum Status {
    Connecting,
    Ready,
    Failed(String),
}

#[derive(Debug, Clone, Copy)]
pub enum DeviceAction {
    Ring,
    Ping,
    Browse,
    Pair,
    AcceptPair,
    RejectPair,
    Unpair,
}

#[derive(Debug, Clone)]
pub enum Message {
    Connect,
    Connected(Result<Client, String>),
    Refreshed(Result<(Vec<Device>, Vec<Transfer>), String>),
    Daemon(Event),
    DragEnter,
    DragLeave,
    Dropped(String, Vec<u8>),
    ChooseFiles,
    Send(Payload),
    SendClipboard,
    ClipboardRead(Option<String>),
    Run(DeviceAction),
    Done(Result<Option<String>, String>),
    CancelTransfer(u64),
    StartReply(String),
    ReplyInput(String),
    SendReply,
    CancelReply,
    Dismiss(String),
    SearchAgain,
    Open(PathBuf),
    CloseToast(toaster::ToastId),
    ToggleAbout,
    LaunchUrl(String),
}

impl cosmic::Application for App {
    type Executor = cosmic::executor::Default;
    type Flags = ();
    type Message = Message;

    const APP_ID: &'static str = "io.github.namaray.Pocketlink";

    fn core(&self) -> &cosmic::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut cosmic::Core {
        &mut self.core
    }

    fn init(core: cosmic::Core, _flags: ()) -> (Self, Task<cosmic::Action<Message>>) {
        let about = About::default()
            .name("Pocketlink")
            .icon(icon::from_svg_bytes(APP_ICON))
            .version(env!("CARGO_PKG_VERSION"))
            .comments("Your phone and your computer, together.")
            .links([("Source code", env!("CARGO_PKG_REPOSITORY"))])
            .license(env!("CARGO_PKG_LICENSE"));

        let app = App {
            core,
            about,
            nav: nav_bar::Model::default(),
            toasts: toaster::Toasts::new(Message::CloseToast),
            client: None,
            status: Status::Connecting,
            devices: Vec::new(),
            transfers: BTreeMap::new(),
            sent_texts: Vec::new(),
            drag_hover: false,
            refreshing: false,
            refresh_again: false,
            reply: None,
        };
        (app, cosmic::task::message(Message::Connect))
    }

    fn header_end(&self) -> Vec<Element<'_, Message>> {
        vec![
            widget::button::icon(icon::from_name("view-refresh-symbolic"))
                .on_press(Message::SearchAgain)
                .into(),
            widget::button::icon(icon::from_name("help-about-symbolic"))
                .on_press(Message::ToggleAbout)
                .into(),
        ]
    }

    fn nav_model(&self) -> Option<&nav_bar::Model> {
        (self.devices.len() > 1).then_some(&self.nav)
    }

    fn on_nav_select(&mut self, id: nav_bar::Id) -> Task<cosmic::Action<Message>> {
        self.nav.activate(id);
        self.reply = None;
        Task::none()
    }

    fn context_drawer(&self) -> Option<context_drawer::ContextDrawer<'_, Message>> {
        self.core.window.show_context.then(|| {
            context_drawer::about(
                &self.about,
                |url| Message::LaunchUrl(url.to_owned()),
                Message::ToggleAbout,
            )
        })
    }

    fn subscription(&self) -> Subscription<Message> {
        Subscription::run(daemon_events)
    }

    fn update(&mut self, message: Message) -> Task<cosmic::Action<Message>> {
        match message {
            Message::Connect => {
                self.status = Status::Connecting;
                return cosmic::task::future(async {
                    Message::Connected(Client::connect().await.map_err(friendly))
                });
            }

            Message::Connected(Ok(client)) => {
                self.client = Some(client);
                return self.refresh();
            }

            Message::Connected(Err(err)) => self.status = Status::Failed(err),

            Message::Refreshed(result) => {
                self.refreshing = false;
                match result {
                    Ok((devices, transfers)) => {
                        self.status = Status::Ready;
                        self.set_devices(devices);
                        for t in transfers {
                            self.transfers.insert(t.id, t);
                        }
                    }
                    Err(err) => self.status = Status::Failed(err),
                }
                if std::mem::take(&mut self.refresh_again) {
                    return self.refresh();
                }
            }

            Message::Daemon(event) => return self.on_event(event),

            Message::DragEnter => self.drag_hover = true,

            Message::DragLeave => self.drag_hover = false,

            Message::Dropped(mime, data) => {
                self.drag_hover = false;
                if let Some(payload) = drop::parse(&mime, &data) {
                    return self.update(Message::Send(payload));
                }
            }

            Message::ChooseFiles => {
                return cosmic::task::future(async {
                    let dialog = cosmic::dialog::file_chooser::open::Dialog::new();
                    let files = match dialog.open_files().await {
                        Ok(response) => response
                            .urls()
                            .iter()
                            .filter_map(|u| u.to_file_path().ok())
                            .collect(),
                        Err(_) => Vec::new(),
                    };
                    Message::Send(Payload::Files(files))
                });
            }

            Message::Send(payload) => return self.send(payload),

            Message::SendClipboard => {
                return cosmic::iced::clipboard::read()
                    .map(|text| cosmic::Action::App(Message::ClipboardRead(text)));
            }

            Message::ClipboardRead(text) => {
                let Some(text) = text.filter(|t| !t.trim().is_empty()) else {
                    return self.toast("Your clipboard is empty");
                };
                let name = self.active().map(|d| d.name.clone()).unwrap_or_default();
                return self.call(move |client, id| async move {
                    client.send_clipboard(&id, &text).await?;
                    Ok(Some(format!("Copied to {name}'s clipboard")))
                });
            }

            Message::Run(action) => {
                let name = self.active().map(|d| d.name.clone()).unwrap_or_default();
                return self.call(move |client, id| async move {
                    Ok(match action {
                        DeviceAction::Ring => {
                            client.ring(&id).await?;
                            Some(format!("Ringing {name}"))
                        }
                        DeviceAction::Ping => {
                            client.ping(&id).await?;
                            Some(format!("Pinged {name}"))
                        }
                        DeviceAction::Browse => {
                            client.browse(&id).await?;
                            Some(format!("Opening {name}'s files…"))
                        }
                        DeviceAction::Pair => {
                            client.pair(&id).await?;
                            None
                        }
                        DeviceAction::AcceptPair => {
                            client.accept_pair(&id).await?;
                            Some(format!("Paired with {name}"))
                        }
                        DeviceAction::RejectPair => {
                            client.reject_pair(&id).await?;
                            None
                        }
                        DeviceAction::Unpair => {
                            client.unpair(&id).await?;
                            Some(format!("Unpaired {name}"))
                        }
                    })
                });
            }

            Message::Done(Ok(Some(text))) => return self.toast(text),

            Message::Done(Ok(None)) => {}

            Message::Done(Err(err)) => return self.toast(err),

            Message::CancelTransfer(tid) => {
                return self.call(move |client, _| async move {
                    client.cancel_transfer(tid).await?;
                    Ok(None)
                });
            }

            Message::StartReply(reply_id) => self.reply = Some((reply_id, String::new())),

            Message::ReplyInput(text) => {
                if let Some((_, draft)) = &mut self.reply {
                    *draft = text;
                }
            }

            Message::SendReply => {
                let Some((reply_id, text)) = self.reply.take() else {
                    return Task::none();
                };
                if text.trim().is_empty() {
                    return Task::none();
                }
                return self.call(move |client, id| async move {
                    client.reply_notification(&id, &reply_id, &text).await?;
                    Ok(Some("Reply sent".to_owned()))
                });
            }

            Message::CancelReply => self.reply = None,

            Message::Dismiss(nid) => {
                return self.call(move |client, id| async move {
                    client.dismiss_notification(&id, &nid).await?;
                    Ok(None)
                });
            }

            Message::SearchAgain => {
                let Some(client) = self.client.clone() else {
                    return self.update(Message::Connect);
                };
                return Task::batch([
                    cosmic::task::future(async move {
                        Message::Done(client.refresh().await.map(|()| None).map_err(friendly))
                    }),
                    self.refresh(),
                ]);
            }

            Message::Open(path) => {
                if let Err(err) = open::that_detached(&path) {
                    tracing::warn!(?path, %err, "failed to open");
                }
            }

            Message::CloseToast(id) => self.toasts.remove(id),

            Message::ToggleAbout => {
                self.core.window.show_context = !self.core.window.show_context;
            }

            Message::LaunchUrl(url) => {
                if let Err(err) = open::that_detached(&url) {
                    tracing::warn!(%url, %err, "failed to open");
                }
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let content = match (&self.status, self.active()) {
            (Status::Connecting, _) => empty_state(
                widget::progress_bar::indeterminate_circular().size(48.0).into(),
                "Starting Pocketlink…",
                "",
                None,
            ),
            (Status::Failed(err), _) => view_failed(err),
            (Status::Ready, None) => view_no_devices(),
            (Status::Ready, Some(device)) => self.view_device(device),
        };

        let page = widget::container(content)
            .max_width(760)
            .width(Length::Fill)
            .padding(cosmic::theme::spacing().space_l)
            .apply(widget::container)
            .center_x(Length::Fill)
            .apply(widget::scrollable);

        let page = widget::dnd_destination(page, drop::mime_types())
            .on_enter(|_, _, _| Message::DragEnter)
            .on_leave(|| Message::DragLeave)
            .on_finish(|mime, data, _, _, _| Message::Dropped(mime, data));

        toaster(&self.toasts, page)
    }
}

impl App {
    fn active(&self) -> Option<&Device> {
        match self.nav.active_data::<String>() {
            Some(id) => self.devices.iter().find(|d| &d.id == id),
            None => self.devices.first(),
        }
    }

    /// Run a daemon call for the active device and report the result as a toast.
    fn call<F, Fut>(&self, f: F) -> Task<cosmic::Action<Message>>
    where
        F: FnOnce(Client, String) -> Fut + Send + 'static,
        Fut: Future<Output = pocketlink_core::Result<Option<String>>> + Send,
    {
        let (Some(client), Some(device)) = (self.client.clone(), self.active()) else {
            return Task::none();
        };
        let id = device.id.clone();
        cosmic::task::future(async move { Message::Done(f(client, id).await.map_err(friendly)) })
    }

    fn on_event(&mut self, event: Event) -> Task<cosmic::Action<Message>> {
        match event {
            Event::Changed => self.refresh(),
            Event::Transfer(t) => {
                let toast = match (&t.state, t.direction) {
                    (TransferState::Done, Direction::Incoming) => {
                        Some(format!("Received {}", t.name))
                    }
                    (TransferState::Failed { .. }, Direction::Outgoing) => {
                        Some(format!("Couldn't send {}", t.name))
                    }
                    _ => None,
                };
                let first_time = self
                    .transfers
                    .get(&t.id)
                    .is_none_or(|old| old.state != t.state);
                self.transfers.insert(t.id, t);
                match toast.filter(|_| first_time) {
                    Some(text) => self.toast(text),
                    None => Task::none(),
                }
            }
            Event::ClipboardReceived { text, .. } => {
                cosmic::iced::clipboard::write(text).map(cosmic::Action::App)
            }
            Event::PairRequest { device_id, .. } => {
                // Jump to the device that's asking.
                let entity = self
                    .nav
                    .iter()
                    .find(|e| self.nav.data::<String>(*e) == Some(&device_id));
                if let Some(entity) = entity {
                    self.nav.activate(entity);
                }
                self.refresh()
            }
        }
    }

    fn refresh(&mut self) -> Task<cosmic::Action<Message>> {
        if self.refreshing {
            self.refresh_again = true;
            return Task::none();
        }
        let Some(client) = self.client.clone() else {
            return Task::none();
        };
        self.refreshing = true;
        cosmic::task::future(async move {
            // Signals arrive in bursts; let them settle before re-reading everything.
            tokio::time::sleep(Duration::from_millis(100)).await;
            let result = async { Ok((client.devices().await?, client.transfers().await?)) }.await;
            Message::Refreshed(result.map_err(friendly))
        })
    }

    fn set_devices(&mut self, devices: Vec<Device>) {
        let same_list = devices.len() == self.devices.len()
            && devices
                .iter()
                .zip(&self.devices)
                .all(|(a, b)| a.id == b.id && a.name == b.name);
        if !same_list {
            let active = self.nav.active_data::<String>().cloned();
            self.nav.clear();
            for device in &devices {
                let item = self
                    .nav
                    .insert()
                    .text(device.name.clone())
                    .icon(icon::from_name(device_icon(device)))
                    .data::<String>(device.id.clone());
                if active.as_ref() == Some(&device.id) {
                    item.activate();
                }
            }
            if self.nav.active_data::<String>().is_none() {
                let first = self.nav.iter().next();
                if let Some(first) = first {
                    self.nav.activate(first);
                }
            }
        }
        self.devices = devices;
    }

    fn send(&mut self, payload: Payload) -> Task<cosmic::Action<Message>> {
        let Some(device) = self.active() else {
            return Task::none();
        };
        if !device.can(caps::SHARE) {
            let why = if device.reachable {
                format!("Pair with {} first", device.name)
            } else {
                format!("{} is offline", device.name)
            };
            return self.toast(why);
        }

        match payload {
            Payload::Files(paths) => {
                let over_bluetooth = device.transport == Some(Transport::Bluetooth);
                let (files, folders): (Vec<_>, Vec<_>) = paths.into_iter().partition(|p| !p.is_dir());
                let mut tasks = Vec::new();
                if !folders.is_empty() {
                    tasks.push(self.toast("Folders can't be sent yet, so they were skipped"));
                }
                let total: u64 = files
                    .iter()
                    .filter_map(|f| std::fs::metadata(f).ok())
                    .map(|m| m.len())
                    .sum();
                if over_bluetooth && total > BLUETOOTH_SLOW_BYTES {
                    tasks.push(self.toast(format!(
                        "Sending {} over Bluetooth will be slow. Wi-Fi is much faster.",
                        bytes(total)
                    )));
                }
                if !files.is_empty() {
                    tasks.push(self.call(move |client, id| async move {
                        client.send_files(&id, &files).await?;
                        Ok(None)
                    }));
                }
                Task::batch(tasks)
            }
            Payload::Text(text) => {
                let name = device.name.clone();
                self.sent_texts.insert(0, ellipsize(&text, 60));
                self.sent_texts.truncate(10);
                self.call(move |client, id| async move {
                    client.send_text(&id, &text).await?;
                    Ok(Some(format!("Sent to {name}")))
                })
            }
        }
    }

    fn toast(&mut self, text: impl Into<String>) -> Task<cosmic::Action<Message>> {
        self.toasts
            .push(toaster::Toast::new(text))
            .map(cosmic::Action::App)
    }

    fn view_device<'a>(&'a self, device: &'a Device) -> Element<'a, Message> {
        let spacing = cosmic::theme::spacing();
        let mut page = widget::column::with_capacity(6)
            .push(hero(device))
            .spacing(spacing.space_l);

        if device.paired() {
            page = page
                .push(self.drop_zone(device))
                .push(quick_actions(device))
                .push_maybe(self.notifications(device))
                .push_maybe(self.activity(device))
                .push(
                    widget::row::with_capacity(2)
                        .push(widget::Space::new().width(Length::Fill))
                        .push(
                            widget::button::destructive("Unpair")
                                .on_press(Message::Run(DeviceAction::Unpair)),
                        ),
                );
        } else {
            page = page.push(pairing(device));
        }
        page.into()
    }

    fn drop_zone<'a>(&'a self, device: &'a Device) -> Element<'a, Message> {
        let hover = self.drag_hover && device.reachable;

        let (icon_name, title, subtitle) = match (device.reachable, hover) {
            (false, _) => (
                "network-offline-symbolic",
                format!("{} is offline", device.name),
                "Open Pocketlink on your phone and make sure it's on the same Wi-Fi.".to_owned(),
            ),
            (true, true) => (
                "document-send-symbolic",
                format!("Drop to send to {}", device.name),
                String::new(),
            ),
            (true, false) => (
                "document-send-symbolic",
                "Drag files here to send them".to_owned(),
                "Photos, documents, links or text — anything you drop goes straight to your phone."
                    .to_owned(),
            ),
        };

        let buttons = widget::row::with_capacity(2)
            .push(
                widget::button::suggested("Choose files…")
                    .leading_icon(icon::from_name("document-open-symbolic"))
                    .on_press_maybe(device.can(caps::SHARE).then_some(Message::ChooseFiles)),
            )
            .push(
                widget::button::standard("Send clipboard")
                    .leading_icon(icon::from_name("edit-paste-symbolic"))
                    .on_press_maybe(device.can(caps::CLIPBOARD).then_some(Message::SendClipboard)),
            )
            .spacing(8);

        let inner = widget::column::with_capacity(4)
            .push(icon::from_name(icon_name).size(56))
            .push(widget::text::title3(title))
            .push_maybe((!subtitle.is_empty()).then(|| widget::text::body(subtitle)))
            .push_maybe((!hover).then_some(buttons))
            .align_x(Alignment::Center)
            .spacing(12);

        widget::container(inner)
            .width(Length::Fill)
            .padding([48, 24])
            .align_x(Horizontal::Center)
            .class(cosmic::theme::Container::custom(move |theme| {
                let c = theme.cosmic();
                let accent: Color = c.accent_color().into();
                let (bg, border, width) = if hover {
                    (Color { a: 0.15, ..accent }, accent, 2.0)
                } else {
                    (c.bg_component_color().into(), c.bg_divider().into(), 1.0)
                };
                widget::container::Style {
                    background: Some(Background::Color(bg)),
                    border: Border {
                        color: border,
                        width,
                        radius: c.corner_radii.radius_l.into(),
                    },
                    text_color: Some(c.on_bg_color().into()),
                    icon_color: Some(if hover { accent } else { c.on_bg_color().into() }),
                    ..Default::default()
                }
            }))
            .into()
    }

    fn notifications<'a>(&'a self, device: &'a Device) -> Option<Element<'a, Message>> {
        if device.notifications.is_empty() {
            return None;
        }
        let mut section = settings::section().title("Notifications");
        for n in device.notifications.iter().take(20) {
            section = section.add(self.notification(n));
        }
        Some(section.into())
    }

    fn notification<'a>(&'a self, n: &'a Notification) -> Element<'a, Message> {
        let replying = self
            .reply
            .as_ref()
            .filter(|(rid, _)| Some(rid) == n.reply_id.as_ref());

        let title = match (n.app.is_empty(), n.title.is_empty()) {
            (false, false) => format!("{} · {}", n.app, n.title),
            (false, true) => n.app.clone(),
            _ => n.title.clone(),
        };

        let mut buttons = widget::row::with_capacity(2).spacing(4);
        if let Some(reply_id) = n.reply_id.as_ref().filter(|_| replying.is_none()) {
            buttons = buttons.push(
                widget::button::text("Reply").on_press(Message::StartReply(reply_id.clone())),
            );
        }
        if n.dismissable {
            buttons = buttons.push(
                widget::button::icon(icon::from_name("window-close-symbolic"))
                    .on_press(Message::Dismiss(n.id.clone())),
            );
        }

        let item = settings::item::builder(title)
            .description(n.text.clone())
            .icon(icon::from_name("preferences-system-notifications-symbolic"))
            .control(buttons);

        match replying {
            None => item.into(),
            Some((_, draft)) => widget::column::with_capacity(2)
                .push(item)
                .push(
                    widget::row::with_capacity(3)
                        .push(
                            widget::text_input("Write a reply…", draft)
                                .on_input(Message::ReplyInput)
                                .on_submit(|_| Message::SendReply)
                                .width(Length::Fill),
                        )
                        .push(widget::button::suggested("Send").on_press(Message::SendReply))
                        .push(widget::button::text("Cancel").on_press(Message::CancelReply))
                        .align_y(Alignment::Center)
                        .spacing(8),
                )
                .spacing(8)
                .into(),
        }
    }

    fn activity<'a>(&'a self, device: &'a Device) -> Option<Element<'a, Message>> {
        let transfers: Vec<&Transfer> = self
            .transfers
            .values()
            .rev()
            .filter(|t| t.device_id == device.id)
            .take(12)
            .collect();
        if transfers.is_empty() && self.sent_texts.is_empty() {
            return None;
        }
        let mut section = settings::section().title("Activity");
        for t in transfers {
            section = section.add(transfer_row(t));
        }
        for text in self.sent_texts.iter().take(5) {
            section = section.add(
                settings::item::builder(format!("\u{201c}{text}\u{201d}"))
                    .description("Sent")
                    .icon(icon::from_name("edit-paste-symbolic"))
                    .control(widget::Space::new()),
            );
        }
        Some(section.into())
    }
}

fn transfer_row(t: &Transfer) -> Element<'_, Message> {
    let (icon_name, verb) = match t.direction {
        Direction::Incoming => ("document-save-symbolic", "Received"),
        Direction::Outgoing => ("document-send-symbolic", "Sent"),
    };
    let item = settings::item::builder(t.name.clone()).icon(icon::from_name(icon_name));

    match &t.state {
        TransferState::Active => {
            let fraction = if t.total == 0 { 0.0 } else { t.done as f32 / t.total as f32 };
            item.description(format!("{} of {}", bytes(t.done), bytes(t.total)))
                .control(
                    widget::row::with_capacity(2)
                        .push(
                            widget::progress_bar::determinate_linear(fraction).width(Length::Fixed(140.0)),
                        )
                        .push(
                            widget::button::icon(icon::from_name("process-stop-symbolic"))
                                .on_press(Message::CancelTransfer(t.id)),
                        )
                        .align_y(Alignment::Center)
                        .spacing(8),
                )
                .into()
        }
        TransferState::Done => {
            let item = item.description(format!("{verb} · {}", bytes(t.total)));
            match t.path.as_ref().map(PathBuf::from) {
                Some(path) => {
                    let folder = path.parent().map(PathBuf::from).unwrap_or_default();
                    item.control(
                        widget::row::with_capacity(2)
                            .push(widget::button::text("Open").on_press(Message::Open(path)))
                            .push(widget::button::text("Show in folder").on_press(Message::Open(folder)))
                            .spacing(4),
                    )
                    .into()
                }
                None => item.control(icon::from_name("emblem-ok-symbolic").size(16)).into(),
            }
        }
        TransferState::Failed { error } => item
            .description(format!("Failed: {error}"))
            .control(icon::from_name("dialog-error-symbolic").size(16))
            .into(),
        TransferState::Cancelled => item
            .description("Cancelled")
            .control(widget::Space::new())
            .into(),
    }
}

fn pairing(device: &Device) -> Element<'_, Message> {
    let (title, body, code, buttons): (String, String, Option<&str>, Vec<Element<'_, Message>>) =
        match &device.pair {
            PairState::RequestedByPeer { code } => (
                format!("{} wants to pair", device.name),
                "Check that both screens show the same code, then accept.".to_owned(),
                Some(code),
                vec![
                    widget::button::suggested("Accept")
                        .on_press(Message::Run(DeviceAction::AcceptPair))
                        .into(),
                    widget::button::standard("Reject")
                        .on_press(Message::Run(DeviceAction::RejectPair))
                        .into(),
                ],
            ),
            PairState::Requested { code } => (
                format!("Waiting for {}…", device.name),
                "Accept the request on your phone. Both screens should show this code.".to_owned(),
                Some(code),
                vec![
                    widget::button::standard("Cancel")
                        .on_press(Message::Run(DeviceAction::RejectPair))
                        .into(),
                ],
            ),
            _ => (
                format!("Pair with {}?", device.name),
                "Pairing lets you send files, see notifications and more. You only do it once."
                    .to_owned(),
                None,
                vec![
                    widget::button::suggested("Pair")
                        .leading_icon(icon::from_name("emblem-shared-symbolic"))
                        .on_press_maybe(device.reachable.then_some(Message::Run(DeviceAction::Pair)))
                        .into(),
                ],
            ),
        };

    let code = code.filter(|c| !c.is_empty()).map(|code| {
        widget::container(widget::text::title1(spaced(code)))
            .padding([12, 24])
            .class(cosmic::theme::Container::Card)
    });

    widget::container(
        widget::column::with_capacity(5)
            .push(icon::from_name("emblem-shared-symbolic").size(56))
            .push(widget::text::title3(title))
            .push(widget::text::body(body).align_x(Horizontal::Center))
            .push_maybe(code)
            .push(widget::row::with_children(buttons).spacing(8))
            .align_x(Alignment::Center)
            .spacing(16),
    )
    .width(Length::Fill)
    .padding([48, 24])
    .class(cosmic::theme::Container::Card)
    .into()
}

fn hero(device: &Device) -> Element<'_, Message> {
    let spacing = cosmic::theme::spacing();
    const RING: f32 = 88.0;

    let battery: Element<_> = match device.battery {
        Some(b) => stack![
            widget::progress_bar::determinate_circular(f32::from(b.charge) / 100.0)
                .size(RING)
                .bar_height(8.0),
            widget::column::with_capacity(2)
                .push_maybe(
                    b.charging
                        .then(|| icon::from_name("battery-full-charging-symbolic").size(16))
                )
                .push(widget::text::title4(format!("{}%", b.charge)))
                .align_x(Alignment::Center)
                .apply(widget::container)
                .center(RING),
        ]
        .into(),
        None => widget::container(icon::from_name(device_icon(device)).size(48))
            .center(RING)
            .into(),
    };

    let online = device.reachable;
    let dot = widget::container(widget::Space::new().width(10).height(10)).class(
        cosmic::theme::Container::custom(move |theme| {
            let c = theme.cosmic();
            let color: Color = if online {
                c.success_color().into()
            } else {
                c.bg_divider().into()
            };
            widget::container::Style {
                background: Some(Background::Color(color)),
                border: Border {
                    radius: 5.0.into(),
                    ..Default::default()
                },
                ..Default::default()
            }
        }),
    );

    let status_text = match (online, device.transport) {
        (true, Some(Transport::Bluetooth)) => "Connected over Bluetooth".to_owned(),
        (true, _) => match &device.address {
            Some(address) => format!("Connected over Wi-Fi · {address}"),
            None => "Connected".to_owned(),
        },
        (false, _) => "Offline".to_owned(),
    };

    let mut status = widget::row::with_capacity(4)
        .push(dot)
        .push(widget::text::body(status_text))
        .align_y(Alignment::Center)
        .spacing(8);

    if let Some(b) = device.battery.filter(|b| b.charging) {
        status = status.push(widget::text::body(format!("· Charging, {}%", b.charge)));
    }
    if let Some(signal) = &device.signal {
        status = status
            .push(icon::from_name(signal_icon(signal.strength)).size(16))
            .push(widget::text::body(signal.network.clone()));
    }

    widget::row::with_capacity(2)
        .push(battery)
        .push(
            widget::column::with_capacity(2)
                .push(widget::text::title1(device.name.clone()))
                .push(status)
                .spacing(spacing.space_xxs),
        )
        .align_y(Alignment::Center)
        .spacing(spacing.space_l)
        .into()
}

fn quick_actions(device: &Device) -> Element<'_, Message> {
    let tile = |icon_name: &'static str, label: &'static str, capability: &str, action| {
        widget::button::custom(
            widget::column::with_capacity(2)
                .push(icon::from_name(icon_name).size(32))
                .push(widget::text::body(label))
                .align_x(Alignment::Center)
                .spacing(8)
                .width(Length::Fill),
        )
        .class(cosmic::theme::Button::Standard)
        .padding([20, 12])
        .width(Length::Fill)
        .on_press_maybe(device.can(capability).then_some(Message::Run(action)))
        .into()
    };

    widget::column::with_capacity(2)
        .push(widget::text::heading("Quick actions"))
        .push(
            widget::row::with_children([
                tile("audio-volume-high-symbolic", "Ring phone", caps::RING, DeviceAction::Ring),
                tile("folder-symbolic", "Browse files", caps::SFTP, DeviceAction::Browse),
                tile("emblem-synchronizing-symbolic", "Ping", caps::PING, DeviceAction::Ping),
            ])
            .spacing(12),
        )
        .spacing(8)
        .into()
}

fn view_failed(err: &str) -> Element<'_, Message> {
    widget::column::with_capacity(2)
        .push(empty_state(
            icon::from_name("dialog-warning-symbolic").size(64).into(),
            "Pocketlink's background service isn't running",
            "Start it with “pocketlink-daemon”, or install Pocketlink with “just install” so it \
             starts by itself.",
            Some(widget::button::suggested("Try again").on_press(Message::Connect).into()),
        ))
        .push(widget::text::caption(err))
        .align_x(Alignment::Center)
        .spacing(12)
        .into()
}

fn view_no_devices() -> Element<'static, Message> {
    empty_state(
        icon::from_name("phone-symbolic").size(64).into(),
        "Looking for your phone…",
        "Open the Pocketlink (or KDE Connect) app on your phone and connect it to the same Wi-Fi \
         as this computer. It will show up here.",
        Some(
            widget::button::standard("Search again")
                .leading_icon(icon::from_name("view-refresh-symbolic"))
                .on_press(Message::SearchAgain)
                .into(),
        ),
    )
}

fn empty_state<'a>(
    art: Element<'a, Message>,
    title: &'a str,
    body: &'a str,
    action: Option<Element<'a, Message>>,
) -> Element<'a, Message> {
    widget::column::with_capacity(4)
        .push(art)
        .push(widget::text::title2(title))
        .push_maybe(
            (!body.is_empty()).then(|| widget::text::body(body).align_x(Horizontal::Center)),
        )
        .push_maybe(action)
        .align_x(Alignment::Center)
        .spacing(16)
        .padding([96, 24])
        .width(Length::Fill)
        .into()
}

fn device_icon(device: &Device) -> &'static str {
    match device.kind {
        DeviceKind::Phone | DeviceKind::Unknown => "phone-symbolic",
        DeviceKind::Tablet => "tablet-symbolic",
        DeviceKind::Laptop => "computer-laptop-symbolic",
        DeviceKind::Desktop => "computer-symbolic",
        DeviceKind::Tv => "video-display-symbolic",
    }
}

fn signal_icon(bars: u8) -> &'static str {
    match bars {
        0 => "network-cellular-signal-none-symbolic",
        1 => "network-cellular-signal-weak-symbolic",
        2 => "network-cellular-signal-ok-symbolic",
        3 => "network-cellular-signal-good-symbolic",
        _ => "network-cellular-signal-excellent-symbolic",
    }
}

/// "ABCD1234" → "ABCD 1234", easier to compare at a glance.
fn spaced(code: &str) -> String {
    if code.len() == 8 && code.is_ascii() {
        format!("{} {}", &code[..4], &code[4..])
    } else {
        code.to_owned()
    }
}

fn bytes(n: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if n < 1000 {
        return format!("{n} B");
    }
    let mut value = n as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit < UNITS.len() - 1 {
        value /= 1000.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

fn ellipsize(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or_default().trim();
    if line.chars().count() <= max && !text.trim().contains('\n') {
        line.to_owned()
    } else {
        let cut: String = line.chars().take(max).collect();
        format!("{}…", cut.trim_end())
    }
}

/// The server's message, without the D-Bus error name in front.
fn friendly(err: pocketlink_core::Error) -> String {
    match err {
        pocketlink_core::Error::MethodError(_, Some(message), _) => message,
        other => other.to_string(),
    }
}

/// Daemon signals, forever. Reconnects if the daemon restarts.
fn daemon_events() -> impl futures::Stream<Item = Message> {
    cosmic::iced::stream::channel(32, |mut tx: futures::channel::mpsc::Sender<Message>| async move {
        loop {
            match Client::connect().await {
                Ok(client) => match client.events().await {
                    Ok(events) => {
                        let mut events = std::pin::pin!(events);
                        while let Some(event) = events.next().await {
                            if tx.send(Message::Daemon(event)).await.is_err() {
                                return;
                            }
                        }
                    }
                    Err(err) => tracing::warn!(%err, "can't listen to the daemon"),
                },
                Err(err) => tracing::warn!(%err, "can't reach the session bus"),
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_sizes() {
        assert_eq!(bytes(512), "512 B");
        assert_eq!(bytes(1_500), "1.5 KB");
        assert_eq!(bytes(52_000_000), "52 MB");
    }

    #[test]
    fn spaces_codes() {
        assert_eq!(spaced("ABCD1234"), "ABCD 1234");
        assert_eq!(spaced("ABC"), "ABC");
    }
}
