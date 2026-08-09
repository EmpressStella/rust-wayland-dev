//! Small playerctl-backed media card.
//!
//! The card is hidden when no MPRIS player is available and refreshed once per second.

use crate::helpers; // Shared helper for running shell commands
use async_channel::unbounded;
use gtk4::prelude::*;
use gtk4::{Align, Box, Button, Label, Orientation};

struct MediaSnapshot {
    status: String,
    title: String,
    artist: String,
}

fn parse_media_snapshot(out: &[u8]) -> Option<MediaSnapshot> {
    let raw = String::from_utf8_lossy(out);
    let parts: Vec<&str> = raw.trim().split(";;").collect();
    if parts.len() < 3 {
        return None;
    }

    Some(MediaSnapshot {
        status: parts[0].to_string(),
        title: parts[1].to_string(),
        artist: parts[2].to_string(),
    })
}

/// Builds the media card and starts its background metadata poller.
pub fn build() -> Box {
    let container = Box::builder()
        .orientation(Orientation::Vertical)
        .css_classes(vec!["media-card"])
        .visible(false) // Start hidden
        .halign(Align::Fill)
        .build();

    let title_label = Label::builder()
        .label("Unknown Title")
        .css_classes(vec!["media-title"])
        .wrap(true)
        .max_width_chars(25) // Approx width before wrapping/cutting off
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .halign(Align::Center)
        .build();

    let artist_label = Label::builder()
        .label("Unknown Artist")
        .css_classes(vec!["media-artist"])
        .wrap(true)
        .max_width_chars(25)
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .halign(Align::Center)
        .build();

    let controls = Box::builder()
        .orientation(Orientation::Horizontal)
        .halign(Align::Center)
        .spacing(10)
        .margin_top(5)
        .build();

    let btn_prev = Button::builder()
        .label("⏮")
        .css_classes(vec!["media-btn"])
        .build();
    let btn_play = Button::builder()
        .label("⏸")
        .css_classes(vec!["media-btn", "play-btn"])
        .build();
    let btn_next = Button::builder()
        .label("⏭")
        .css_classes(vec!["media-btn"])
        .build();

    btn_prev.connect_clicked(|_| {
        helpers::run_command("playerctl", &["previous"]);
    });
    btn_next.connect_clicked(|_| {
        helpers::run_command("playerctl", &["next"]);
    });

    let btn_play_clone = btn_play.clone();
    btn_play.connect_clicked(move |_| {
        helpers::run_command("playerctl", &["play-pause"]);
    });

    controls.append(&btn_prev);
    controls.append(&btn_play);
    controls.append(&btn_next);

    container.append(&title_label);
    container.append(&artist_label);
    container.append(&controls);

    // Keep playerctl off the GTK thread and deliver results through GLib.
    let container_poll = container.clone();
    let title_poll = title_label.clone();
    let artist_poll = artist_label.clone();
    let play_btn_poll = btn_play_clone.clone();

    let (tx, rx) = unbounded::<Option<MediaSnapshot>>();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
            let output = helpers::get_output(
                "playerctl",
                &["metadata", "--format", "{{status}};;{{title}};;{{artist}}"],
            );
            let parsed = output.as_deref().and_then(parse_media_snapshot);
            if tx.send_blocking(parsed).is_err() {
                break;
            }
        }
    });

    glib::MainContext::default().spawn_local(async move {
        while let Ok(snapshot) = rx.recv().await {
            match snapshot {
                Some(data) => {
                    container_poll.set_visible(true);
                    title_poll.set_label(&data.title);
                    artist_poll.set_label(&data.artist);
                    if data.status == "Playing" {
                        play_btn_poll.set_label("⏸");
                    } else {
                        play_btn_poll.set_label("▶");
                    }
                }
                None => {
                    container_poll.set_visible(false);
                }
            }
        }
    });

    container
}
