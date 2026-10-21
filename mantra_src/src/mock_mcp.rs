//! `mantra mock-mcp`: a stdio MCP server standing in for a real one (Blender's, say) in `--demo`
//! and the tests, so every backend's MCP path runs end to end with nothing installed: `--demo`
//! registers it as `mcp_servers.demo` and gives it to Solo and every role (`main.rs`, `app.rs`).
//!
//! Two tools, shaped like what a 3D app's server offers: `scene_info` answers with text (a scene
//! of three objects), `render_preview` with an image — a small PNG drawn and encoded right here
//! (stored deflate blocks, no compression crate needed) — and a line of text. The JSON-RPC side is
//! `mcp_bridge::serve`, the same loop Mantra's own bridge runs on.

use crate::mcp_bridge::{serve, text_content, ToolServer};
use crate::util::b64;
use serde_json::{json, Value};
use tokio::io::BufReader;

/// The name the server gives in `initialize` (`mantra mcp test` prints it).
const SERVER_NAME: &str = "mantra-mock-mcp";

/// What `scene_info` says: the demo's whole scene.
pub(crate) const SCENE: &str = "Scene \"demo\": 3 objects\n- Cube (mesh) at (0, 0, 1), 2 m, material \"saffron\"\n- Camera at (7.4, -6.9, 5.0), looking at the cube, 50 mm\n- Light (point, 1000 W) at (4.1, 1.0, 5.9)\nFrames 1-250, current 1. Render engine: Eevee, 1920×1080.";

struct Demo;

impl ToolServer for Demo {
    async fn list(&mut self) -> Value {
        json!([
            {
                "name": "scene_info",
                "description": "The demo scene: its objects, the camera and the frame range.",
                "inputSchema": {"type": "object", "properties": {}},
            },
            {
                "name": "render_preview",
                "description": "Renders a small preview of the demo scene from its camera and returns it as an image.",
                "inputSchema": {"type": "object", "properties": {
                    "width": {"type": "integer", "description": "pixels, 8 to 256 (default 64)"},
                    "height": {"type": "integer", "description": "pixels, 8 to 256 (default 48)"},
                }},
            },
        ])
    }

    async fn call(&mut self, name: &str, args: Value) -> Value {
        match name {
            "scene_info" => json!({"content": [text_content(SCENE)], "isError": false}),
            "render_preview" => {
                let side = |k: &str, default: u64| args.get(k).and_then(|v| v.as_u64()).unwrap_or(default).clamp(8, 256) as u32;
                let (w, h) = (side("width", 64), side("height", 48));
                let png = preview_png(w, h);
                json!({"content": [
                    text_content(&format!("Rendered a {w}×{h} preview of the demo scene from Camera, frame 1.")),
                    {"type": "image", "data": b64(&png), "mimeType": "image/png"},
                ], "isError": false})
            }
            other => json!({"content": [text_content(&format!("unknown tool {other}: there is scene_info and render_preview"))], "isError": true}),
        }
    }
}

pub async fn run() {
    serve(BufReader::new(tokio::io::stdin()), &mut tokio::io::stdout(), SERVER_NAME, true, &mut Demo).await;
}

/// The preview: a dusk-blue sky fading to a grey floor, and the saffron cube in the middle.
fn preview_png(w: u32, h: u32) -> Vec<u8> {
    let mut rgb = Vec::with_capacity((w * h * 3) as usize);
    let (cx, cy, half) = (w as i64 / 2, h as i64 / 2, (w.min(h) as i64 / 4).max(2));
    for y in 0..h as i64 {
        for x in 0..w as i64 {
            let px = if (x - cx).abs() <= half && (y - cy).abs() <= half {
                // lit from the upper left: the cube's top-left half a shade brighter
                if (x - cx) + (y - cy) < 0 { [0xF4, 0xA2, 0x3C] } else { [0xD9, 0x82, 0x1E] }
            } else if y < h as i64 * 2 / 3 {
                let t = (y * 255 / (h as i64)) as u8;
                [0x1F_u8.saturating_add(t / 6), 0x2A_u8.saturating_add(t / 5), 0x4A_u8.saturating_add(t / 3)]
            } else {
                [0x6B, 0x6B, 0x70]
            };
            rgb.extend_from_slice(&px);
        }
    }
    encode_png(w, h, &rgb)
}

/// An 8-bit RGB PNG of `rgb` (row-major, 3 bytes a pixel): IHDR, one IDAT holding a zlib stream
/// of stored (uncompressed) deflate blocks, IEND.
pub(crate) fn encode_png(w: u32, h: u32, rgb: &[u8]) -> Vec<u8> {
    let row = w as usize * 3;
    let mut raw = Vec::with_capacity((row + 1) * h as usize);
    for line in rgb.chunks(row) {
        raw.push(0); // filter type None
        raw.extend_from_slice(line);
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, block) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len())); // BFINAL on the last; BTYPE 00 = stored
        let len = block.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(block);
    }
    z.extend_from_slice(&adler32(&raw).to_be_bytes());
    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 2, 0, 0, 0]); // 8-bit, truecolour, deflate, adaptive filtering, no interlace
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, data) in [(b"IHDR", ihdr.as_slice()), (b"IDAT", z.as_slice()), (b"IEND", &[][..])] {
        png.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let start = png.len();
        png.extend_from_slice(kind);
        png.extend_from_slice(data);
        let crc = crc32(&png[start..]);
        png.extend_from_slice(&crc.to_be_bytes());
    }
    png
}

fn adler32(data: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for byte in data {
        a = (a + *byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

/// CRC-32 (ISO-HDLC, the one PNG chunks use), bit by bit: a few kilobytes a call, no table needed.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::config::McpServer;

    /// The stand-in as a real process for the tests that need one (`mantra mcp test`): this very
    /// test binary, re-run as just `serve_when_asked` with `MANTRA_TEST_MOCK_MCP` set. libtest's own
    /// banner line on stdout is not JSON, which an MCP client skips.
    pub(crate) fn test_server() -> McpServer {
        let exe = std::env::current_exe().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        let args = ["--exact", "mock_mcp::tests::serve_when_asked", "--nocapture", "--test-threads=1", "-q"];
        McpServer { command: exe, args: args.iter().map(|a| a.to_string()).collect(), env: [("MANTRA_TEST_MOCK_MCP".to_string(), "1".to_string())].into(), enabled: true }
    }

    /// Not a test: what `test_server` runs. Without the variable it does nothing.
    #[test]
    fn serve_when_asked() {
        if std::env::var_os("MANTRA_TEST_MOCK_MCP").is_none() {
            return;
        }
        if let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() {
            rt.block_on(run());
        }
        // Gone before libtest prints its verdict on the stdout the client is reading.
        std::process::exit(0);
    }

    fn png_size(png: &[u8]) -> (u32, u32) {
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert_eq!(&png[12..16], b"IHDR");
        (u32::from_be_bytes([png[16], png[17], png[18], png[19]]), u32::from_be_bytes([png[20], png[21], png[22], png[23]]))
    }

    /// A PNG any decoder takes: the signature, IHDR with the asked size, every chunk's CRC right,
    /// and the zlib stream's own checksum over exactly the filtered rows.
    #[test]
    fn the_preview_is_a_well_formed_png() {
        assert_eq!(crc32(b"IEND"), 0xAE42_6082, "the CRC every PNG ends with");
        assert_eq!(adler32(b"Wikipedia"), 0x11E6_0398);
        let png = preview_png(64, 48);
        assert_eq!(png_size(&png), (64, 48));
        let mut at = 8;
        let mut kinds = vec![];
        while at < png.len() {
            let len = u32::from_be_bytes([png[at], png[at + 1], png[at + 2], png[at + 3]]) as usize;
            let body = &png[at + 4..at + 8 + len];
            let crc = u32::from_be_bytes([png[at + 8 + len], png[at + 9 + len], png[at + 10 + len], png[at + 11 + len]]);
            assert_eq!(crc32(body), crc, "chunk {}", String::from_utf8_lossy(&body[..4]));
            kinds.push(String::from_utf8_lossy(&body[..4]).into_owned());
            if &body[..4] == b"IDAT" {
                let z = &body[4..];
                assert_eq!(&z[..2], &[0x78, 0x01]);
                // one stored block here: header byte, LEN, NLEN, then the rows
                let n = u16::from_le_bytes([z[3], z[4]]) as usize;
                assert_eq!((z[2], n, !u16::from_le_bytes([z[5], z[6]])), (1, (64 * 3 + 1) * 48, n as u16));
                let raw = &z[7..7 + n];
                assert_eq!(&z[7 + n..], &adler32(raw).to_be_bytes());
            }
            at += 12 + len;
        }
        assert_eq!(kinds, vec!["IHDR", "IDAT", "IEND"]);
        // a big one spans several stored blocks
        let big = encode_png(256, 256, &vec![7u8; 256 * 256 * 3]);
        assert_eq!(png_size(&big), (256, 256));
        assert_eq!(b64(b"Ma"), "TWE=");
    }

    /// Through the real client over an in-memory pipe: the handshake, both tools listed, the scene
    /// as text, the preview as an image of the asked size, an unknown tool as an error result.
    #[tokio::test]
    async fn the_client_lists_and_calls_the_demo_tools() {
        let (client_end, server_end) = tokio::io::duplex(1 << 20);
        let (srv_rd, mut srv_wr) = tokio::io::split(server_end);
        tokio::spawn(async move { serve(BufReader::new(srv_rd), &mut srv_wr, SERVER_NAME, true, &mut Demo).await });
        let (rd, wr) = tokio::io::split(client_end);
        let mut c = crate::mcp::Client::over(rd, wr);
        c.initialize().await.unwrap();
        assert_eq!(c.protocol, "2025-06-18");
        assert!(c.server.starts_with(SERVER_NAME), "{}", c.server);
        let names: Vec<String> = c.list_tools().await.unwrap().iter().filter_map(|t| t["name"].as_str().map(str::to_string)).collect();
        assert_eq!(names, vec!["scene_info", "render_preview"]);
        let scene = c.call_tool("scene_info", json!({})).await.unwrap();
        assert!(!scene.is_error && scene.text.contains("3 objects") && scene.images.is_empty(), "{scene:?}");
        let shot = c.call_tool("render_preview", json!({"width": 32, "height": 9000})).await.unwrap();
        assert!(!shot.is_error && shot.text.contains("32×256"), "{shot:?}");
        assert_eq!(shot.images.len(), 1);
        assert_eq!(shot.images[0].0, "image/png");
        let bytes = crate::web::crypto::b64url_decode(&shot.images[0].1).expect("valid base64");
        assert_eq!(png_size(&bytes), (32, 256), "clamped to 256");
        let bad = c.call_tool("explode", json!({})).await.unwrap();
        assert!(bad.is_error && bad.text.contains("scene_info"), "{bad:?}");
    }
}
