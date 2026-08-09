//! Static system information card.
//!
//! Values are collected once when the sidebar opens; this card does not poll.

use crate::helpers::get_stdout;
use gtk4::prelude::*;
use gtk4::{Align, Box, Label, Orientation};

/// Builds the system information card.
pub fn build() -> Box {
    let container = Box::builder()
        .orientation(Orientation::Vertical)
        .css_classes(vec!["sysinfo-card"])
        .valign(Align::Center)
        .vexpand(true)
        .build();

    let kernel = get_stdout("uname", &["-r"]);

    let shell_path = std::env::var("SHELL").unwrap_or_else(|_| "Unknown".to_string());
    let shell = shell_path.split('/').next_back().unwrap_or("zsh");

    let wm = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "Wayland".to_string());

    let pkgs = crate::helpers::pkg_count();

    let uptime = get_stdout("uptime", &["-p"]).replace("up ", "");

    let rows = vec![
        ("  Kernel", kernel),
        ("  Shell", shell.to_string()),
        ("  WM", wm),
        ("📦 Pkgs", pkgs),
        ("  Uptime", uptime),
    ];

    for (icon_label, value) in rows {
        let row = Box::builder()
            .orientation(Orientation::Horizontal)
            .spacing(10)
            .build();

        let key = Label::builder()
            .label(icon_label)
            .css_classes(vec!["sysinfo-key"])
            .halign(Align::Start)
            .hexpand(true)
            .build();

        let val = Label::builder()
            .label(&value)
            .css_classes(vec!["sysinfo-value"])
            .halign(Align::End)
            .build();

        row.append(&key);
        row.append(&val);
        container.append(&row);
    }

    container
}
