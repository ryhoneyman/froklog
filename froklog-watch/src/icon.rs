//! froklog's own artwork, used for the tray icon and the window icon.
//!
//! These are the same PNGs the server serves as its favicon, so the icon in
//! the tray matches the icon on the tab — one thing to recognise, not two.
//! They are 16×16 because that is what a favicon is; the dock will scale them.

/// The titlebar and dock want more than 16 pixels. Nearest-neighbour upscale,
/// which suits pixel art: it stays crisp instead of going soft.
pub const WINDOW: &[u8] = include_bytes!("../assets/icons/froklog-watch-64.png");

/// Multi-size tray sets, tinted from the 256×256 frog in upstream's
/// froklog.ico (the 16×16 favicons went blurry on HiDPI panels). State
/// colors follow the Windows client's tray language: green = streaming,
/// orange = wants to stream but the connection is down, gray = idle.
macro_rules! tray_set {
    ($state:literal) => {
        [
            include_bytes!(concat!("../assets/tray/frog-", $state, "-16.png")) as &[u8],
            include_bytes!(concat!("../assets/tray/frog-", $state, "-24.png")),
            include_bytes!(concat!("../assets/tray/frog-", $state, "-32.png")),
            include_bytes!(concat!("../assets/tray/frog-", $state, "-48.png")),
            include_bytes!(concat!("../assets/tray/frog-", $state, "-64.png")),
        ]
    };
}
pub const TRAY_GREEN: [&[u8]; 5] = tray_set!("green");
pub const TRAY_ORANGE: [&[u8]; 5] = tray_set!("orange");
pub const TRAY_GRAY: [&[u8]; 5] = tray_set!("gray");

pub struct Rgba {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

/// Decode one of the PNGs above to straight RGBA.
///
/// Both consumers demand four bytes per pixel and will silently refuse an icon
/// that is anything else — which is how a palette PNG (what most image tools
/// emit for a small image) turns into a window with no icon at all. So expand
/// whatever came in rather than trusting the artwork to be RGBA.
pub fn decode(bytes: &'static [u8]) -> Rgba {
    let mut decoder = png::Decoder::new(bytes);
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().expect("icon is a valid png");
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).expect("icon decodes");
    buf.truncate(info.buffer_size());

    // EXPAND gets us to 8-bit colour, but not necessarily to an alpha channel.
    let pixels = match info.color_type {
        png::ColorType::Rgba => buf,
        png::ColorType::Rgb => buf
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buf
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buf.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => unreachable!("EXPAND removes the palette"),
    };
    Rgba {
        width: info.width,
        height: info.height,
        pixels,
    }
}

/// The tray speaks ARGB32 in network byte order, not RGBA.
pub fn to_argb(icon: &Rgba) -> Vec<u8> {
    let mut out = Vec::with_capacity(icon.pixels.len());
    for px in icon.pixels.chunks_exact(4) {
        out.extend_from_slice(&[px[3], px[0], px[1], px[2]]);
    }
    out
}
