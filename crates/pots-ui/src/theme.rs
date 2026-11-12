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
