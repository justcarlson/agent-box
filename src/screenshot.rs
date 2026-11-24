//! Pure-Rust X11 root window screenshot capture to PNG.

use anyhow::{bail, Context, Result};
use std::fs::{self, File};
use std::io::BufWriter;
use std::os::unix::net::UnixStream;
use std::path::Path;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, ImageOrder, Visualtype};
use x11rb::rust_connection::{DefaultStream, RustConnection};

/// Capture the root window of X display `:{display}` using `xauth_path` and
/// write an RGB PNG to `out_path`.
pub fn capture_root_png(display: u32, xauth_path: &Path, out_path: &Path) -> Result<()> {
    let xauth_bytes =
        fs::read(xauth_path).with_context(|| format!("read xauth {}", xauth_path.display()))?;
    let (auth_name, auth_data) = parse_xauth(&xauth_bytes, display)
        .with_context(|| format!("parse xauth {}", xauth_path.display()))?;

    let sock_path = format!("/tmp/.X11-unix/X{display}");
    let unix = UnixStream::connect(&sock_path)
        .with_context(|| format!("connect to X11 display :{display} ({sock_path})"))?;
    let (stream, _) = DefaultStream::from_unix_stream(unix).context("initialize X11 stream")?;
    let conn = RustConnection::connect_to_stream_with_auth_info(stream, 0, auth_name, auth_data)
        .with_context(|| format!("authenticate to X11 display :{display}"))?;

    let setup = conn.setup();
    let screen = setup.roots.first().context("X11 screen 0 not found")?;
    let width = screen.width_in_pixels;
    let height = screen.height_in_pixels;

    let visual = find_visual(screen, screen.root_visual)
        .context("root visual not found in X11 screen setup")?;

    let reply = conn
        .get_image(ImageFormat::Z_PIXMAP, screen.root, 0, 0, width, height, !0)
        .context("send X11 GetImage")?
        .reply()
        .context("read X11 GetImage reply")?;

    let format = setup
        .pixmap_formats
        .iter()
        .find(|f| f.depth == reply.depth)
        .with_context(|| format!("no X11 pixmap format for depth {}", reply.depth))?;

    let msb_first = setup.image_byte_order == ImageOrder::MSB_FIRST;
    let rgb = decode_zpixmap_to_rgb(
        &reply.data,
        width,
        height,
        format.bits_per_pixel,
        format.scanline_pad,
        msb_first,
        visual.red_mask,
        visual.green_mask,
        visual.blue_mask,
    )?;

    write_rgb_png(out_path, width as u32, height as u32, &rgb)
}

fn find_visual(screen: &x11rb::protocol::xproto::Screen, visual_id: u32) -> Option<&Visualtype> {
    screen
        .allowed_depths
        .iter()
        .flat_map(|d| d.visuals.iter())
        .find(|v| v.visual_id == visual_id)
}

/// Parse an Xauthority binary file and extract the `MIT-MAGIC-COOKIE-1` entry
/// matching `display` (or wildcard empty display number).
pub(crate) fn parse_xauth(bytes: &[u8], display: u32) -> Result<(Vec<u8>, Vec<u8>)> {
    let display_str = display.to_string();
    let mut pos = 0usize;
    let mut fallback: Option<(Vec<u8>, Vec<u8>)> = None;

    while pos < bytes.len() {
        if pos + 2 > bytes.len() {
            bail!("truncated xauth entry header");
        }
        let _family = u16::from_be_bytes([bytes[pos], bytes[pos + 1]]);
        pos += 2;

        let _address = read_xauth_field(bytes, &mut pos)?;
        let number = read_xauth_field(bytes, &mut pos)?;
        let name = read_xauth_field(bytes, &mut pos)?;
        let data = read_xauth_field(bytes, &mut pos)?;

        if name == b"MIT-MAGIC-COOKIE-1" {
            if number.is_empty() || number == display_str.as_bytes() {
                return Ok((name.to_vec(), data.to_vec()));
            }
            if fallback.is_none() {
                fallback = Some((name.to_vec(), data.to_vec()));
            }
        }
    }

    fallback.context("no MIT-MAGIC-COOKIE-1 entry in xauth file")
}

fn read_xauth_field<'a>(bytes: &'a [u8], pos: &mut usize) -> Result<&'a [u8]> {
    if *pos + 2 > bytes.len() {
        bail!("truncated xauth field length");
    }
    let len = u16::from_be_bytes([bytes[*pos], bytes[*pos + 1]]) as usize;
    *pos += 2;
    if *pos + len > bytes.len() {
        bail!("truncated xauth field payload");
    }
    let slice = &bytes[*pos..*pos + len];
    *pos += len;
    Ok(slice)
}

fn extract_channel(pixel: u32, mask: u32) -> u8 {
    if mask == 0 {
        return 0;
    }
    let shift = mask.trailing_zeros();
    let max = mask >> shift;
    let val = (pixel & mask) >> shift;
    if max == 255 {
        val as u8
    } else {
        ((val * 255 + max / 2) / max) as u8
    }
}

/// Convert X11 ZPixmap pixel data (24-bit or 32-bit BGRX/RGBX, LSBFirst or MSBFirst)
/// into packed 8-bit RGB bytes using the screen visual's color masks.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_zpixmap_to_rgb(
    data: &[u8],
    width: u16,
    height: u16,
    bits_per_pixel: u8,
    scanline_pad: u8,
    msb_first: bool,
    red_mask: u32,
    green_mask: u32,
    blue_mask: u32,
) -> Result<Vec<u8>> {
    let bytes_per_pixel: usize = match bits_per_pixel {
        32 => 4,
        24 => 3,
        other => bail!("unsupported X11 bits_per_pixel: {other}"),
    };
    let w = width as usize;
    let h = height as usize;
    let row_bytes = w * bytes_per_pixel;
    let pad = (scanline_pad / 8).max(1) as usize;
    let stride = row_bytes.div_ceil(pad) * pad;
    let needed = stride * h;
    if data.len() < needed {
        bail!(
            "X11 image buffer too small: got {} bytes, expected at least {needed} ({w}x{h} @ {bits_per_pixel}bpp)",
            data.len()
        );
    }

    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        let row = &data[y * stride..y * stride + row_bytes];
        for px in row.chunks_exact(bytes_per_pixel) {
            let val = match (bytes_per_pixel, msb_first) {
                (4, false) => u32::from_le_bytes([px[0], px[1], px[2], px[3]]),
                (4, true) => u32::from_be_bytes([px[0], px[1], px[2], px[3]]),
                (3, false) => (px[0] as u32) | ((px[1] as u32) << 8) | ((px[2] as u32) << 16),
                (3, true) => ((px[0] as u32) << 16) | ((px[1] as u32) << 8) | (px[2] as u32),
                _ => unreachable!(),
            };
            rgb.push(extract_channel(val, red_mask));
            rgb.push(extract_channel(val, green_mask));
            rgb.push(extract_channel(val, blue_mask));
        }
    }
    Ok(rgb)
}

pub(crate) fn write_rgb_png(out_path: &Path, width: u32, height: u32, rgb: &[u8]) -> Result<()> {
    if let Some(parent) = out_path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .with_context(|| format!("create directory {}", parent.display()))?;
    }
    let file = File::create(out_path)
        .with_context(|| format!("create screenshot {}", out_path.display()))?;
    let mut encoder = png::Encoder::new(BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().context("write PNG header")?;
    writer
        .write_image_data(rgb)
        .context("write PNG pixel data")?;
    writer.finish().context("finish PNG file")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_xauth_written_by_stack() {
        let path = std::env::temp_dir().join(format!("agent-box-xauth-{}.bin", std::process::id()));
        crate::stack::write_xauth(&path).unwrap();
        let bytes = fs::read(&path).unwrap();
        let (name, data) = parse_xauth(&bytes, 100).unwrap();
        assert_eq!(name, b"MIT-MAGIC-COOKIE-1");
        assert_eq!(data.len(), 16);
        fs::remove_file(&path).unwrap();
    }

    #[test]
    fn parses_matching_display_entry_from_multi_entry_xauth() {
        let mut buf = Vec::new();
        let push = |buf: &mut Vec<u8>, num: &[u8], name: &[u8], cookie: &[u8]| {
            buf.extend(0x0100u16.to_be_bytes());
            buf.extend(4u16.to_be_bytes());
            buf.extend(b"host");
            buf.extend((num.len() as u16).to_be_bytes());
            buf.extend(num);
            buf.extend((name.len() as u16).to_be_bytes());
            buf.extend(name);
            buf.extend((cookie.len() as u16).to_be_bytes());
            buf.extend(cookie);
        };
        push(&mut buf, b"99", b"MIT-MAGIC-COOKIE-1", b"aaaaaaaaaaaaaaaa");
        push(&mut buf, b"105", b"MIT-MAGIC-COOKIE-1", b"bbbbbbbbbbbbbbbb");
        let (name, data) = parse_xauth(&buf, 105).unwrap();
        assert_eq!(name, b"MIT-MAGIC-COOKIE-1");
        assert_eq!(data, b"bbbbbbbbbbbbbbbb");
    }

    #[test]
    fn decodes_bgrx_32bit_lsb_and_rgbx_msb_and_24bit_padded() {
        // 2x1 32-bit BGRX little-endian: pixel0=(R=0x11,G=0x22,B=0x33), pixel1=(R=0xaa,G=0xbb,B=0xcc)
        let bgrx = [0x33, 0x22, 0x11, 0xff, 0xcc, 0xbb, 0xaa, 0x00];
        let rgb = decode_zpixmap_to_rgb(
            &bgrx, 2, 1, 32, 32, false, 0x00ff0000, 0x0000ff00, 0x000000ff,
        )
        .unwrap();
        assert_eq!(rgb, vec![0x11, 0x22, 0x33, 0xaa, 0xbb, 0xcc]);

        // 1x1 32-bit RGBX little-endian (red in low byte, blue in byte 2).
        let rgbx = [0x10, 0x20, 0x30, 0xff];
        let rgb = decode_zpixmap_to_rgb(
            &rgbx, 1, 1, 32, 32, false, 0x000000ff, 0x0000ff00, 0x00ff0000,
        )
        .unwrap();
        assert_eq!(rgb, vec![0x10, 0x20, 0x30]);

        // 1x2 24-bit packed LSB with 32-bit scanline pad (3 bytes pixel + 1 pad byte per row).
        let bgr24 = [0x03, 0x02, 0x01, 0x99, 0x06, 0x05, 0x04, 0x88];
        let rgb = decode_zpixmap_to_rgb(
            &bgr24, 1, 2, 24, 32, false, 0x00ff0000, 0x0000ff00, 0x000000ff,
        )
        .unwrap();
        assert_eq!(rgb, vec![0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);

        // 1x1 32-bit MSB-first (big-endian XRGB).
        let xrgb_be = [0x00, 0xde, 0xad, 0xbe];
        let rgb = decode_zpixmap_to_rgb(
            &xrgb_be, 1, 1, 32, 32, true, 0x00ff0000, 0x0000ff00, 0x000000ff,
        )
        .unwrap();
        assert_eq!(rgb, vec![0xde, 0xad, 0xbe]);
    }

    #[test]
    fn writes_and_decodes_valid_png() {
        let path =
            std::env::temp_dir().join(format!("agent-box-shot-test-{}.png", std::process::id()));
        let pixels = vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        write_rgb_png(&path, 2, 2, &pixels).unwrap();

        let raw = fs::read(&path).unwrap();
        assert_eq!(&raw[..8], b"\x89PNG\r\n\x1a\n");

        let decoder = png::Decoder::new(std::io::BufReader::new(File::open(&path).unwrap()));
        let mut reader = decoder.read_info().unwrap();
        let mut buf = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut buf).unwrap();
        assert_eq!((info.width, info.height), (2, 2));
        assert_eq!(&buf[..info.buffer_size()], pixels.as_slice());

        fs::remove_file(&path).unwrap();
    }
}
