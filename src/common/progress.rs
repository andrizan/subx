use indicatif::{ProgressBar, ProgressStyle};

/// Progress-bar helper (Phase 0: default template).
pub fn bar(len: u64, msg: &str) -> ProgressBar {
    let bar = ProgressBar::new(len);
    let style = ProgressStyle::with_template("{msg} [{bar:30.cyan/blue}] {pos}/{len} ({eta})")
        .unwrap_or_else(|_| ProgressStyle::default_bar())
        .progress_chars("=>-");
    bar.set_style(style);
    bar.set_message(msg.to_string());
    bar
}
