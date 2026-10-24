// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::PathBuf;
use std::time::Duration;

use cosmic::app::context_drawer;
use cosmic::iced::alignment::Horizontal;
use cosmic::iced::futures::{SinkExt, StreamExt};
use cosmic::iced::widget::stack;
use cosmic::iced::{Alignment, Background, Border, Color, Length, Subscription};
use cosmic::prelude::*;
use cosmic::widget::{self, about::About, icon, nav_bar, settings, toaster};
use pocketlink_core::{Client, Device, DeviceKind, Event};

use crate::drop::{self, Payload};

const APP_ICON: &[u8] = include_bytes!("../resources/icon.svg");

pub struct App {
    core: cosmic::Core,
    about: About,
    nav: nav_bar::Model,
    toasts: toaster::Toasts<Message>,
    client: Option<Client>,
    status: Status,
    devices: Vec<Device>,
    /// Newest first.
    activity: Vec<Activity>,
    next_activity: u64,
    drag_hover: bool,
    refreshing: bool,
    refresh_again: bool,
}

enum Status {
    Connecting,
    Ready,
    Failed(String),
}

struct Activity {
    id: u64,
    kind: ActivityKind,
}

enum ActivityKind {
    Sending(String),
    Sent(String),
    Failed(String, String),
    Received(PathBuf),
}

#[derive(Debug, Clone, Copy)]
pub enum DeviceAction {
    Ring,
    Ping,
    Browse,
}

#[derive(Debug, Clone)]
pub enum Message {
    Connect,
    Connected(Result<Client, String>),
    Refreshed(Result<Vec<Device>, String>),
    Daemon(Event),
    DragEnter,
    DragLeave,
    Dropped(String, Vec<u8>),
    ChooseFiles,
    Send(Payload),
    SendClipboard,
    ClipboardRead(Option<String>),
    SendFinished(u64, Result<(), String>),
    Run(DeviceAction),
    RunFinished(Result<String, String>),
    Open(PathBuf),
    OpenPairing,
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
            .comments("A friendlier way to use KDE Connect.")
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
            activity: Vec::new(),
            next_activity: 0,
            drag_hover: false,
            refreshing: false,
            refresh_again: false,
        };
        (app, cosmic::task::message(Message::Connect))
    }

    fn header_end(&self) -> Vec<Element<'_, Message>> {
        vec![
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
                    Message::Connected(Client::connect().await.map_err(|e| e.to_string()))
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
                    Ok(devices) => {
                        self.status = Status::Ready;
                        self.set_devices(devices);
                    }
                    Err(err) => self.status = Status::Failed(err),
                }
                if std::mem::take(&mut self.refresh_again) {
                    return self.refresh();
                }
            }

            Message::Daemon(Event::Changed) => return self.refresh(),

            Message::Daemon(Event::ShareReceived { url, .. }) => {
                let Some(path) = url::Url::parse(&url).ok().and_then(|u| u.to_file_path().ok())
                else {
                    return Task::none();
                };
                let name = file_name(&path);
                self.log(ActivityKind::Received(path));
                return self.toast(format!("Received {name}"));
            }

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
                    match dialog.open_files().await {
                        Ok(response) => {
                            let files = response
                                .urls()
                                .iter()
                                .filter_map(|u| u.to_file_path().ok())
                                .collect();
                            Message::Send(Payload::Files(files))
                        }
                        Err(_) => Message::Send(Payload::Files(Vec::new())),
                    }
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
                let (Some(client), Some(device)) = (self.client.clone(), self.active()) else {
                    return Task::none();
                };
                let id = device.id.clone();
                let entry = self.log(ActivityKind::Sending("Clipboard".into()));
                return cosmic::task::future(async move {
                    let result = client.send_clipboard(&id, &text).await;
                    Message::SendFinished(entry, result.map_err(|e| e.to_string()))
                });
            }

            Message::SendFinished(entry, result) => {
                let Some(activity) = self.activity.iter_mut().find(|a| a.id == entry) else {
                    return Task::none();
                };
                let ActivityKind::Sending(label) = &activity.kind else {
                    return Task::none();
                };
                let label = label.clone();
                match result {
                    Ok(()) => activity.kind = ActivityKind::Sent(label),
                    Err(err) => {
                        activity.kind = ActivityKind::Failed(label.clone(), err);
                        return self.toast(format!("Couldn't send {label}"));
                    }
                }
            }

            Message::Run(action) => {
                let (Some(client), Some(device)) = (self.client.clone(), self.active()) else {
                    return Task::none();
                };
                let id = device.id.clone();
                let name = device.name.clone();
                return cosmic::task::future(async move {
                    let result = match action {
                        DeviceAction::Ring => client
                            .ring(&id)
                            .await
                            .map(|()| format!("Ringing {name}")),
                        DeviceAction::Ping => client
                            .ping(&id)
                            .await
                            .map(|()| format!("Pinged {name}")),
                        DeviceAction::Browse => client
                            .browse(&id)
                            .await
                            .map(|_| format!("Opening {name}'s files")),
                    };
                    Message::RunFinished(result.map_err(|e| e.to_string()))
                });
            }

            Message::RunFinished(Ok(text)) => return self.toast(text),

            Message::RunFinished(Err(err)) => return self.toast(format!("That didn't work: {err}")),

            Message::Open(path) => {
                if let Err(err) = open::that_detached(&path) {
                    tracing::warn!(?path, %err, "failed to open");
                }
            }

            Message::OpenPairing => {
                if let Err(err) = std::process::Command::new("kdeconnect-app").spawn() {
                    return self.toast(format!("Couldn't open KDE Connect: {err}"));
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
            (Status::Connecting, _) => self.view_connecting(),
            (Status::Failed(err), _) => self.view_failed(err),
            (Status::Ready, None) => self.view_no_devices(),
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
            tokio::time::sleep(Duration::from_millis(150)).await;
            Message::Refreshed(client.devices().await.map_err(|e| e.to_string()))
        })
    }

    fn set_devices(&mut self, mut devices: Vec<Device>) {
        // Reachable devices first, then by name.
        devices.sort_by(|a, b| b.reachable.cmp(&a.reachable).then(a.name.cmp(&b.name)));

        let same_list = devices.len() == self.devices.len()
            && devices
                .iter()
                .zip(&self.devices)
                .all(|(a, b)| a.id == b.id && a.name == b.name && a.reachable == b.reachable);
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
        let (Some(client), Some(device)) = (self.client.clone(), self.active()) else {
            return Task::none();
        };
        if !device.reachable {
            return self.toast(format!("{} is offline", device.name));
        }
        let id = device.id.clone();

        match payload {
            Payload::Files(paths) => {
                let (files, folders): (Vec<_>, Vec<_>) = paths.into_iter().partition(|p| !p.is_dir());
                let mut tasks = Vec::new();
                if !folders.is_empty() {
                    tasks.push(self.toast("Folders can't be sent yet, so they were skipped"));
                }
                if !files.is_empty() {
                    let entry = self.log(ActivityKind::Sending(drop::describe(&files)));
                    tasks.push(cosmic::task::future(async move {
                        let result = client.send_files(&id, &files).await;
                        Message::SendFinished(entry, result.map_err(|e| e.to_string()))
                    }));
                }
                Task::batch(tasks)
            }
            Payload::Text(text) => {
                let label = ellipsize(&text, 40);
                let entry = self.log(ActivityKind::Sending(format!("\u{201c}{label}\u{201d}")));
                cosmic::task::future(async move {
                    let result = client.send_text(&id, &text).await;
                    Message::SendFinished(entry, result.map_err(|e| e.to_string()))
                })
            }
        }
    }

    fn log(&mut self, kind: ActivityKind) -> u64 {
        let id = self.next_activity;
        self.next_activity += 1;
        self.activity.insert(0, Activity { id, kind });
        self.activity.truncate(50);
        id
    }

    fn toast(&mut self, text: impl Into<String>) -> Task<cosmic::Action<Message>> {
        self.toasts
            .push(toaster::Toast::new(text))
            .map(cosmic::Action::App)
    }

    fn view_connecting(&self) -> Element<'_, Message> {
        empty_state(
            widget::progress_bar::indeterminate_circular().size(48.0).into(),
            "Looking for KDE Connect…",
            "",
            None,
        )
    }

    fn view_failed<'a>(&'a self, err: &'a str) -> Element<'a, Message> {
        empty_state(
            icon::from_name("dialog-warning-symbolic").size(64).into(),
            "Can't reach KDE Connect",
            "Pocketlink needs the KDE Connect service running on this computer. \
             Install it with “sudo apt install kdeconnect”, then try again.",
            Some(widget::button::suggested("Try again").on_press(Message::Connect).into()),
        )
        .apply(|e| {
            widget::column::with_children([e, widget::text::caption(err).into()])
                .align_x(Alignment::Center)
                .spacing(12)
                .into()
        })
    }

    fn view_no_devices(&self) -> Element<'_, Message> {
        empty_state(
            icon::from_name("phone-symbolic").size(64).into(),
            "No phone paired yet",
            "Install KDE Connect on your phone (Play Store or F-Droid), connect it to the \
             same Wi-Fi as this computer, then pair the two.",
            Some(
                widget::button::suggested("Pair a phone")
                    .on_press(Message::OpenPairing)
                    .into(),
            ),
        )
    }

    fn view_device<'a>(&'a self, device: &'a Device) -> Element<'a, Message> {
        let spacing = cosmic::theme::spacing();

        widget::column::with_capacity(4)
            .push(hero(device))
            .push(self.drop_zone(device))
            .push(quick_actions(device))
            .push_maybe(self.activity_list())
            .spacing(spacing.space_l)
            .into()
    }

    fn drop_zone<'a>(&'a self, device: &'a Device) -> Element<'a, Message> {
        let hover = self.drag_hover && device.reachable;
        let can_send = device.reachable && device.has_plugin("kdeconnect_share");

        let (icon_name, title, subtitle) = match (device.reachable, hover) {
            (false, _) => (
                "network-offline-symbolic",
                format!("{} is offline", device.name),
                "Open KDE Connect on your phone and make sure it's on the same network.".to_owned(),
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
                    .on_press_maybe(can_send.then_some(Message::ChooseFiles)),
            )
            .push(
                widget::button::standard("Send clipboard")
                    .leading_icon(icon::from_name("edit-paste-symbolic"))
                    .on_press_maybe(
                        (device.reachable && device.has_plugin("kdeconnect_clipboard"))
                            .then_some(Message::SendClipboard),
                    ),
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

    fn activity_list(&self) -> Option<Element<'_, Message>> {
        if self.activity.is_empty() {
            return None;
        }
        let mut section = settings::section().title("Activity");
        for entry in self.activity.iter().take(12) {
            let item = match &entry.kind {
                ActivityKind::Sending(label) => settings::item::builder(label.clone())
                    .description("Sending…")
                    .icon(icon::from_name("document-send-symbolic"))
                    .control(widget::progress_bar::indeterminate_circular().size(20.0)),
                ActivityKind::Sent(label) => settings::item::builder(label.clone())
                    .description("Sent")
                    .icon(icon::from_name("emblem-ok-symbolic"))
                    .control(widget::Space::new()),
                ActivityKind::Failed(label, err) => settings::item::builder(label.clone())
                    .description(format!("Failed: {err}"))
                    .icon(icon::from_name("dialog-error-symbolic"))
                    .control(widget::Space::new()),
                ActivityKind::Received(path) => {
                    let folder = path.parent().map(PathBuf::from).unwrap_or_default();
                    settings::item::builder(file_name(path))
                        .description("Received from phone")
                        .icon(icon::from_name("document-save-symbolic"))
                        .control(
                            widget::row::with_capacity(2)
                                .push(
                                    widget::button::text("Open")
                                        .on_press(Message::Open(path.clone())),
                                )
                                .push(
                                    widget::button::text("Show in folder")
                                        .on_press(Message::Open(folder)),
                                )
                                .spacing(4),
                        )
                }
            };
            section = section.add(item);
        }
        Some(section.into())
    }
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

    let mut status = widget::row::with_capacity(4)
        .push(
            widget::container(widget::Space::new().width(10).height(10)).class(cosmic::theme::Container::custom(
                {
                    let online = device.reachable;
                    move |theme| {
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
                    }
                },
            )),
        )
        .push(widget::text::body(if device.reachable {
            "Connected"
        } else {
            "Offline"
        }))
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
    let tile = |icon_name: &'static str, label: &'static str, plugin: &str, action| {
        let enabled = device.reachable && device.has_plugin(plugin);
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
        .on_press_maybe(enabled.then_some(Message::Run(action)))
        .into()
    };

    widget::column::with_capacity(2)
        .push(widget::text::heading("Quick actions"))
        .push(
            widget::row::with_children([
                tile(
                    "audio-volume-high-symbolic",
                    "Ring phone",
                    "kdeconnect_findmyphone",
                    DeviceAction::Ring,
                ),
                tile(
                    "folder-symbolic",
                    "Browse files",
                    "kdeconnect_sftp",
                    DeviceAction::Browse,
                ),
                tile(
                    "emblem-synchronizing-symbolic",
                    "Ping",
                    "kdeconnect_ping",
                    DeviceAction::Ping,
                ),
            ])
            .spacing(12),
        )
        .spacing(8)
        .into()
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

fn file_name(path: &std::path::Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
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

/// Daemon signals, forever. Reconnects if the session bus or daemon goes away.
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
                    Err(err) => tracing::warn!(%err, "can't listen to KDE Connect signals"),
                },
                Err(err) => tracing::warn!(%err, "can't reach the session bus"),
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    })
}
