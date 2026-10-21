//! The vision tools (SPEC §6.1), offered only with `--vision`: `screenshot` captures the screen of
//! the machine bija runs on, `view_image` shows the model an image file. Both hand the image back
//! as bytes (`ToolOutcome::images`); the agent stores it in the session and each wire sends it its
//! own way.
//!
//! bija carries no capture code of its own: it runs the first program that works of a fixed list —
//! `$MANTRA_SCREENSHOT_CMD` first (a template: how tests, and people with a tool of their own, plug
//! in), then grim (Wayland), gnome-screenshot, scrot and ImageMagick's `import` (X11), and
//! `screencapture` (macOS). They run outside the sandbox, because a capture needs the display, but
//! without bija's key variables, and each in its own process group like every command.

use super::sandbox::resolve;
use super::{shell, which, Args};
use crate::types::{human_size, image_from, uuid_v4, ImageData, ToolDef, ToolOutcome, IMAGE_MAX_BYTES};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::process::Command;

pub const TOOLS: [&str; 2] = ["screenshot", "view_image"];

/// The variable naming a capture command template (`{out}`, optional `{region}`).
pub const TEMPLATE_VAR: &str = "MANTRA_SCREENSHOT_CMD";

/// One capture attempt at most; a tool that waits for a click is not one that works.
const CAPTURE_TIMEOUT: Duration = Duration::from_secs(30);

pub fn def(name: &str) -> Option<ToolDef> {
    let d = match name {
        "screenshot" => ToolDef {
            name: name.into(),
            description: "Capture the screen of the machine bija runs on and see it as an image, with its size in pixels. Each capture costs context (about a thousand tokens or more), so capture when you need to look, and pass region when you know where to look.".into(),
            parameters: json!({"type": "object", "properties": {
                "region": {"type": "string", "description": "Only this part of the screen: \"x,y,w,h\" in pixels, e.g. \"0,0,800,600\"."},
                "display": {"type": "string", "description": "Which display or output to capture (an X11 display such as \":0\", or a Wayland output name); default: the current one."}
            }}),
        },
        "view_image" => ToolDef {
            name: name.into(),
            description: "Look at an image file: PNG, JPEG, GIF or WebP, at most 8 MB. Returns the image and its size. The path is relative to the working directory or absolute.".into(),
            parameters: json!({"type": "object", "properties": {
                "path": {"type": "string", "description": "Path of the image file."}
            }, "required": ["path"]}),
        },
        _ => return None,
    };
    Some(d)
}

/// A screen area in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    x: u32,
    y: u32,
    w: u32,
    h: u32,
}

impl Region {
    fn parse(s: &str) -> Result<Region, String> {
        let bad = || format!("`region` must be \"x,y,w,h\" in pixels, e.g. \"0,0,800,600\" (got {s:?})");
        let n: Vec<u32> = s.split(',').map(|p| p.trim().parse::<u32>()).collect::<Result<_, _>>().map_err(|_| bad())?;
        match n[..] {
            [x, y, w, h] if w > 0 && h > 0 => Ok(Region { x, y, w, h }),
            _ => Err(bad()),
        }
    }

    fn csv(self) -> String {
        format!("{},{},{},{}", self.x, self.y, self.w, self.h)
    }
}

/// One way to capture: what it is called in messages, and the command (or why it is not tried).
struct Attempt {
    label: &'static str,
    cmd: Result<Command, String>,
    /// Said beside the result (gnome-screenshot cannot capture a region from the command line).
    note: Option<&'static str>,
}

/// Finds a capture program (`which`, or nothing in tests).
type Find<'a> = &'a dyn Fn(&str) -> Option<PathBuf>;

/// Every capture this machine could try, in order. `template` is `$MANTRA_SCREENSHOT_CMD`.
fn plan(template: Option<&str>, region: Option<Region>, display: Option<&str>, out: &Path, find: Find) -> Vec<Attempt> {
    let installed = |name: &'static str| find(name).map(Command::new).ok_or_else(|| "not installed".to_string());
    let x11 = |mut cmd: Command| {
        if let Some(d) = display {
            cmd.env("DISPLAY", d);
        }
        cmd
    };
    let mut v = Vec::new();
    v.push(Attempt {
        label: TEMPLATE_VAR,
        cmd: match template.map(str::trim).filter(|t| !t.is_empty()) {
            None => Err("not set".into()),
            Some(t) if !t.contains("{out}") => Err("has no {out} in it".into()),
            Some(t) => {
                let line = t.replace("{out}", &shell_quote(&out.to_string_lossy())).replace("{region}", &region.map(|r| shell_quote(&r.csv())).unwrap_or_default());
                let mut cmd = Command::new("sh");
                cmd.arg("-c").arg(line);
                Ok(x11(cmd))
            }
        },
        note: None,
    });
    v.push(Attempt {
        label: "grim",
        cmd: installed("grim").map(|mut c| {
            if let Some(d) = display {
                c.arg("-o").arg(d);
            }
            if let Some(r) = region {
                c.arg("-g").arg(format!("{},{} {}x{}", r.x, r.y, r.w, r.h));
            }
            c.arg(out);
            c
        }),
        note: None,
    });
    v.push(Attempt {
        label: "gnome-screenshot",
        cmd: installed("gnome-screenshot").map(|mut c| {
            c.arg("-f").arg(out);
            x11(c)
        }),
        note: region.map(|_| "the region was not applied: gnome-screenshot captures the whole screen"),
    });
    v.push(Attempt {
        label: "scrot",
        cmd: installed("scrot").map(|mut c| {
            if let Some(r) = region {
                c.arg("-a").arg(r.csv());
            }
            c.arg(out);
            x11(c)
        }),
        note: None,
    });
    v.push(Attempt {
        label: "import",
        cmd: installed("import").map(|mut c| {
            c.arg("-window").arg("root");
            if let Some(r) = region {
                c.arg("-crop").arg(format!("{}x{}+{}+{}", r.w, r.h, r.x, r.y));
            }
            c.arg(out);
            x11(c)
        }),
        note: None,
    });
    v.push(Attempt {
        label: "screencapture",
        cmd: installed("screencapture").map(|mut c| {
            c.arg("-x");
            if let Some(r) = region {
                c.arg("-R").arg(r.csv());
            }
            if let Some(d) = display {
                c.arg("-D").arg(d);
            }
            c.arg(out);
            c
        }),
        note: None,
    });
    v
}

/// `s` as one word for `sh`.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// `screenshot`: the first capture that works, saved by the agent like any tool image.
pub async fn screenshot(a: &Args<'_>, dir: &Path, scrub_env: &[String]) -> Result<ToolOutcome, String> {
    let template = std::env::var(TEMPLATE_VAR).ok();
    capture(a, template.as_deref(), dir, scrub_env, &which).await
}

async fn capture(a: &Args<'_>, template: Option<&str>, dir: &Path, scrub_env: &[String], find: Find<'_>) -> Result<ToolOutcome, String> {
    let region = a.opt_text("region")?.map(str::trim).filter(|r| !r.is_empty()).map(Region::parse).transpose()?;
    let display = a.opt_text("display")?.map(str::trim).filter(|d| !d.is_empty());
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    // Under the session dir, not /tmp: a screen can show anything.
    let out = dir.join(format!(".capture-{}.png", uuid_v4()));
    let mut tried = Vec::new();
    for attempt in plan(template, region, display, &out, find) {
        let mut cmd = match attempt.cmd {
            Ok(cmd) => cmd,
            Err(why) => {
                tried.push(format!("{} ({why})", attempt.label));
                continue;
            }
        };
        for var in scrub_env {
            cmd.env_remove(var);
        }
        let ran = shell::run(cmd, CAPTURE_TIMEOUT, None).await;
        let got = std::fs::read(&out);
        let _ = std::fs::remove_file(&out);
        let failure = match (ran, got) {
            (Err(e), _) => e,
            (Ok(c), _) if c.timed_out => format!("no capture within {} s", CAPTURE_TIMEOUT.as_secs()),
            (Ok(c), Ok(bytes)) if c.exit == Some(0) => match image_from(bytes, "") {
                Ok(img) => {
                    let mut text = format!("Screenshot of the screen of the machine bija runs on ({}, via {})", describe(&img), attempt.label);
                    if let Some(r) = region.filter(|_| attempt.note.is_none()) {
                        text.push_str(&format!(", region {}", r.csv()));
                    }
                    text.push('.');
                    if let Some(note) = attempt.note {
                        text.push_str(&format!(" Note: {note}."));
                    }
                    return Ok(ToolOutcome { ok: true, text, images: vec![img], ..Default::default() });
                }
                Err(why) => why,
            },
            (Ok(c), _) => match last_line(&c.output) {
                Some(said) => format!("exit code {}: {said}", c.exit.unwrap_or(-1)),
                None => format!("exit code {}, no image written", c.exit.unwrap_or(-1)),
            },
        };
        tried.push(format!("{} ({failure})", attempt.label));
    }
    let mut msg = String::from("no screen capture worked here. ");
    if cfg!(target_os = "linux") && std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        msg.push_str("No display is set (DISPLAY, WAYLAND_DISPLAY). ");
    }
    msg.push_str(&format!("Tried: {}. Install grim (Wayland), scrot or ImageMagick (X11), or set {TEMPLATE_VAR} to a command that writes a PNG to {{out}}.", tried.join("; ")));
    Err(msg)
}

/// The last non-blank line a failed capture printed, cut short.
fn last_line(output: &str) -> Option<String> {
    output.lines().rev().map(str::trim).find(|l| !l.is_empty()).map(|l| crate::types::clip(l, 200))
}

/// `image/png, 1920x1080 px, 812.3 KiB` (the pixel size for PNGs, whose header says it).
fn describe(img: &ImageData) -> String {
    let size = human_size(img.bytes.len() as u64);
    match png_size(&img.bytes) {
        Some((w, h)) => format!("{}, {w}x{h} px, {size}", img.mime),
        None => format!("{}, {size}", img.mime),
    }
}

/// A PNG's width and height from its IHDR chunk.
fn png_size(b: &[u8]) -> Option<(u32, u32)> {
    if b.len() < 24 || &b[12..16] != b"IHDR" {
        return None;
    }
    let be = |s: &[u8]| u32::from_be_bytes([s[0], s[1], s[2], s[3]]);
    Some((be(&b[16..20]), be(&b[20..24])))
}

/// `view_image`: any readable regular file (reading is never sandboxed), if it is an image within
/// the cap. The size a file claims is checked first, and the read stops one byte past the cap
/// anyway: a /proc file says it is empty and reads on for gigabytes. A device or a pipe is no
/// image (`/dev/zero` never ends, a FIFO may never start).
pub fn view_image(a: &Args<'_>, cwd: &Path) -> Result<ToolOutcome, String> {
    use std::io::Read;
    let raw = a.path("path")?;
    let path: PathBuf = resolve(cwd, raw);
    let meta = std::fs::metadata(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => format!("no such file: {raw}"),
        _ => format!("{raw}: {e}"),
    })?;
    if meta.is_dir() {
        return Err(format!("{raw} is a directory"));
    }
    if !meta.is_file() {
        return Err(format!("{raw} is not a regular file"));
    }
    if meta.len() > IMAGE_MAX_BYTES {
        return Err(format!("{raw} is {}, over the {} limit for images", human_size(meta.len()), human_size(IMAGE_MAX_BYTES)));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(&path).and_then(|f| f.take(IMAGE_MAX_BYTES + 1).read_to_end(&mut bytes)).map_err(|e| format!("{raw}: {e}"))?;
    if bytes.len() as u64 > IMAGE_MAX_BYTES {
        return Err(format!("{raw} reads on past the {} limit for images", human_size(IMAGE_MAX_BYTES)));
    }
    let img = image_from(bytes, "").map_err(|why| format!("{raw}: {why}"))?;
    Ok(ToolOutcome { ok: true, text: format!("{raw}: {}.", describe(&img)), images: vec![img], ..Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    fn scratch(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("bija-vision-{tag}-{}", uuid_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A PNG header claiming `w`x`h` (the tools look at nothing more).
    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut b = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        b.extend_from_slice(&w.to_be_bytes());
        b.extend_from_slice(&h.to_be_bytes());
        b.extend_from_slice(b"\x08\x06\0\0\0");
        b
    }

    fn args(v: &Value) -> Args<'_> {
        Args::new(v).unwrap()
    }

    /// No capture program installed: only the template is tried.
    fn none(_: &str) -> Option<PathBuf> {
        None
    }

    #[test]
    fn regions_parse_strictly() {
        assert_eq!(Region::parse("0, 10,800,600"), Ok(Region { x: 0, y: 10, w: 800, h: 600 }));
        for bad in ["0,0,800", "0,0,0,600", "a,b,c,d", "-1,0,10,10", "1,2,3,4,5"] {
            assert!(Region::parse(bad).unwrap_err().contains("x,y,w,h"), "{bad}");
        }
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    /// The template runs first, with `{out}` and `{region}` filled in; the capture comes back as an
    /// image and the temporary file is gone.
    #[tokio::test]
    async fn a_capture_through_the_template() {
        let d = scratch("tmpl");
        let fixture = d.join("screen.png");
        std::fs::write(&fixture, png(1920, 1080)).unwrap();
        let tmpl = format!("echo {{region}} > {}; cp {} {{out}}", d.join("region.txt").display(), fixture.display());
        let v = json!({"region": "0,0,800,600"});
        let out = capture(&args(&v), Some(&tmpl), &d.join("images"), &[], &none).await.unwrap();
        assert!(out.ok && out.text.contains("1920x1080 px") && out.text.contains("via MANTRA_SCREENSHOT_CMD") && out.text.contains("region 0,0,800,600"), "{}", out.text);
        assert_eq!(out.images, vec![ImageData { mime: "image/png".into(), bytes: png(1920, 1080) }]);
        assert_eq!(std::fs::read_to_string(d.join("region.txt")).unwrap().trim(), "0,0,800,600");
        assert_eq!(std::fs::read_dir(d.join("images")).unwrap().count(), 0, "the temporary capture is removed");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// When nothing works the error names every attempt and what to install.
    #[tokio::test]
    async fn a_failed_capture_says_what_was_tried() {
        let d = scratch("fail");
        let e = capture(&args(&json!({})), Some("echo 'no screen here' >&2; exit 4 # {out}"), &d, &[], &none).await.unwrap_err();
        assert!(e.contains("MANTRA_SCREENSHOT_CMD (exit code 4: no screen here)") && e.contains("grim (not installed)") && e.contains("Install grim"), "{e}");
        let e = capture(&args(&json!({})), Some("true"), &d, &[], &none).await.unwrap_err();
        assert!(e.contains("has no {out} in it"), "{e}");
        let e = capture(&args(&json!({})), Some("echo hi > {out}"), &d, &[], &none).await.unwrap_err();
        assert!(e.contains("not a PNG, JPEG, GIF or WebP image"), "{e}");
        let e = capture(&args(&json!({"region": "big"})), None, &d, &[], &none).await.unwrap_err();
        assert!(e.contains("x,y,w,h"), "{e}");
        // The tools after the template get the region in their own spelling.
        let fake = |name: &str| Some(PathBuf::from(format!("/usr/bin/{name}")));
        let r = Some(Region { x: 1, y: 2, w: 30, h: 40 });
        let argv: Vec<Vec<String>> = plan(None, r, Some(":1"), Path::new("/o.png"), &fake)
            .into_iter()
            .filter_map(|a| a.cmd.ok())
            .map(|c| c.as_std().get_args().map(|s| s.to_string_lossy().into_owned()).collect())
            .collect();
        assert_eq!(argv[0], ["-o", ":1", "-g", "1,2 30x40", "/o.png"], "grim");
        assert_eq!(argv[1], ["-f", "/o.png"], "gnome-screenshot");
        assert_eq!(argv[2], ["-a", "1,2,30,40", "/o.png"], "scrot");
        assert_eq!(argv[3], ["-window", "root", "-crop", "30x40+1+2", "/o.png"], "import");
        assert_eq!(argv[4], ["-x", "-R", "1,2,30,40", "-D", ":1", "/o.png"], "screencapture");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn view_image_reads_images_and_refuses_the_rest() {
        let d = scratch("view");
        std::fs::write(d.join("shot.png"), png(4, 3)).unwrap();
        std::fs::write(d.join("notes.txt"), "hello").unwrap();
        let big = std::fs::File::create(d.join("big.png")).unwrap();
        big.set_len(IMAGE_MAX_BYTES + 1).unwrap();
        let out = view_image(&args(&json!({"path": "shot.png"})), &d).unwrap();
        assert_eq!(out.text, "shot.png: image/png, 4x3 px, 29 B.");
        assert_eq!(out.images[0].mime, "image/png");
        let abs = d.join("shot.png").display().to_string();
        assert!(view_image(&args(&json!({"path": abs})), Path::new("/")).is_ok(), "absolute paths work from anywhere");
        assert_eq!(view_image(&args(&json!({"path": "notes.txt"})), &d).unwrap_err(), "notes.txt: not a PNG, JPEG, GIF or WebP image");
        assert!(view_image(&args(&json!({"path": "big.png"})), &d).unwrap_err().contains("over the 8.0 MiB limit"));
        assert_eq!(view_image(&args(&json!({"path": "nope.png"})), &d).unwrap_err(), "no such file: nope.png");
        assert!(view_image(&args(&json!({"path": "."})), &d).unwrap_err().contains("is a directory"));
        // a device is no image (`/dev/zero` would read forever), and a /proc file that says it is
        // empty is read only up to the cap
        if Path::new("/dev/zero").exists() {
            assert_eq!(view_image(&args(&json!({"path": "/dev/zero"})), &d).unwrap_err(), "/dev/zero is not a regular file");
        }
        // (pagemap takes reads of 8 bytes at a time only, so the cap's one extra byte may be
        // refused as an invalid argument instead — a refusal either way, never a read to its end)
        if std::fs::File::open("/proc/self/pagemap").is_ok() {
            let e = view_image(&args(&json!({"path": "/proc/self/pagemap"})), &d).unwrap_err();
            assert!(e.contains("reads on past the 8.0 MiB limit") || e.contains("Invalid argument"), "{e}");
        }
        assert!(view_image(&args(&json!({})), &d).unwrap_err().contains("`path` is required"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn both_tools_have_object_schemas() {
        for name in TOOLS {
            let d = def(name).unwrap();
            assert_eq!(d.parameters["type"], "object");
            assert!(d.description.contains("image"), "{name}");
        }
        assert!(def("screenshot").unwrap().description.contains("machine bija runs on"));
        assert!(def("bash").is_none());
    }
}
