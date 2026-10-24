// SPDX-License-Identifier: GPL-3.0-or-later

mod app;
mod drop;

fn main() -> cosmic::iced::Result {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "warn,pocketlink=info".into()),
        )
        .init();

    let settings = cosmic::app::Settings::default()
        .size(cosmic::iced::Size::new(960.0, 720.0))
        .size_limits(cosmic::iced::Limits::NONE.min_width(420.0).min_height(480.0));

    cosmic::app::run::<app::App>(settings, ())
}
