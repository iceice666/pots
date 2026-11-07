//! 色票。

use gpui_kit::{Hsla, rgb};

macro_rules! colors {
    ($($name:ident = $hex:literal;)*) => {$(
        pub fn $name() -> Hsla { rgb($hex).into() }
    )*};
}

colors! {
    // 文字
    text = 0x1f2328;
    text_muted = 0x5c6370;
    text_on_dark = 0xd4d6db;
    text_dim_on_dark = 0x9a9ea7;
    white = 0xffffff;

    // 背景與邊框
    rail_bg = 0x131417;
    rail_item = 0x2a2c31;
    rail_item_hover = 0x34363d;
    sidebar_bg = 0x1c1d21;
    sidebar_hover = 0x26282d;
    sidebar_active = 0x34363d;
    panel_bg = 0xf7f8fa;
    subtle_bg = 0xf6f7f9;
    border = 0xe4e6ea;
    border_strong = 0xd0d4da;
    input_border = 0xc9cdd3;
    track = 0xeceef1;

    // 語意色
    accent = 0x2f5bd3;
    accent_soft = 0xeef3ff;
    accent_text = 0x1e3f9e;
    warn = 0xb45309;
    warn_strong = 0xd97706;
    warn_soft = 0xfff4e5;
    warn_border = 0xe9b673;
    warn_text = 0x92400e;
    attention = 0xf5a524;
    ok = 0x15803d;
    ok_soft = 0xecfdf3;
    danger = 0xe5484d;
    danger_soft = 0xfdecec;
    violet = 0x6d4fc2;
    violet_soft = 0xf3f0ff;
    chip_bg = 0xeceef1;
    chip_text = 0x3d434c;
}
