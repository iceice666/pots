//! Slack-inspired light theme.

use gpui::{Hsla, rgb, rgba};

pub const MONO: &str = "Menlo";

macro_rules! colors {
    ($($name:ident = $value:expr;)*) => {
        $(pub fn $name() -> Hsla { $value.into() })*
    };
}

colors! {
    // Workspace chrome
    topbar = rgb(0x350d36);
    sidebar = rgb(0x3f0e40);
    sidebar_text = rgb(0xcfc3cf);
    sidebar_heading = rgb(0xb8a9b8);
    sidebar_hover = rgba(0xffffff14);
    sidebar_selected = rgb(0x1164a3);
    search_bg = rgba(0xffffff26);
    online = rgb(0x2bac76);
    busy = rgb(0xf2c744);

    // Main pane
    bg = rgb(0xffffff);
    text = rgb(0x1d1c1d);
    muted = rgb(0x616061);
    faint = rgb(0x9a9a9a);
    border = rgb(0xdddddd);
    hover = rgb(0xf8f8f8);
    link = rgb(0x1264a3);
    error = rgb(0xb42318);

    // Markdown
    code_fg = rgb(0xc01343);
    code_bg = rgb(0xf3f3f3);
    code_block_bg = rgb(0xf8f8f8);
    code_block_border = rgb(0xe2e2e2);
    quote_bar = rgb(0xdddddd);
    mention_fg = rgb(0x1264a3);
    mention_bg = rgb(0xe8f5fa);
    table_header = rgb(0xf4f4f4);
    table_stripe = rgb(0xfafafa);
    checkbox_on = rgb(0x007a5a);

    // Reactions
    reaction_bg = rgb(0xf1f1f1);
    reaction_mine_bg = rgb(0xe8f5fa);
    reaction_mine_border = rgb(0x1d9bd1);

    // Threads
    thread_card = rgb(0xf4f5f7);
    thread_card_border = rgb(0xe3e5e8);
    thread_line = rgba(0xffffff40);
    toolbar_bg = rgb(0xffffff);
    panel_border = rgb(0xdddddd);

    // Composer
    composer_toolbar = rgb(0xf8f8f8);
    composer_focus = rgb(0x868686);
    send = rgb(0x007a5a);
    send_hover = rgb(0x148567);
    cursor = rgb(0x1264a3);
    selection = rgba(0x1264a340);
}

/// Points gpui-component's global `Theme` at the palette above, so its
/// widgets look like the rest of Pots. Call after `gpui_component::init`.
///
/// Only the light palette exists here, and nothing calls
/// `Theme::sync_system_appearance`, so the theme never follows the OS into
/// dark mode. Fields with no counterpart above keep the library's light
/// defaults.
pub fn install_component_theme(cx: &mut gpui::App) {
    use gpui::px;
    use gpui_component::{Theme, ThemeMode};

    // Switch mode first: setting it loads that mode's stock palette, which
    // would overwrite colors edited in the same `update`.
    Theme::change(ThemeMode::Light, None, cx);
    Theme::update(cx, |t| {
        // gpui's text defaults, which the Pots views are laid out against.
        // The root plugin turns `font_size` into the window's rem size.
        t.font_family = ".SystemUIFont".into();
        t.font_size = px(16.);
        t.mono_font_family = MONO.into();

        let c = &mut t.colors;
        c.background = bg();
        c.foreground = text();
        c.border = border();
        c.input = border();
        c.ring = composer_focus();
        c.caret = cursor();
        c.selection = selection();
        c.muted = hover();
        c.muted_foreground = muted();
        c.accent = hover();
        c.accent_foreground = text();
        c.popover = bg();
        c.popover_foreground = text();
        c.overlay = rgba(0x00000066).into();
        c.window_border = panel_border();

        c.primary = send();
        c.primary_hover = send_hover();
        c.primary_active = send_hover();
        c.primary_foreground = bg();
        c.button_primary = send();
        c.button_primary_hover = send_hover();
        c.button_primary_active = send_hover();
        c.button_primary_foreground = bg();

        c.link = link();
        c.link_hover = link();
        c.link_active = link();
        c.danger = error();
        c.button_danger = error();
        c.success = online();
        c.warning = busy();

        c.list = bg();
        c.list_hover = hover();
        c.list_even = table_stripe();
        c.list_head = table_header();
        c.table = bg();
        c.table_head = table_header();
        c.table_head_foreground = text();
        c.table_even = table_stripe();
        c.table_hover = hover();
        c.table_row_border = border();

        c.title_bar = topbar();
        c.title_bar_border = topbar();
        c.sidebar = sidebar();
        c.sidebar_foreground = sidebar_text();
        c.sidebar_border = sidebar();
        c.sidebar_accent = sidebar_hover();
        c.sidebar_accent_foreground = sidebar_text();
        c.sidebar_primary = sidebar_selected();
        c.sidebar_primary_foreground = bg();
    });
}
