//! IconForge CLI (M1): core pipeline without the GUI.
//!
//! ```text
//! iconforge base   --out base.ico [--base white|custom] [--size 512]
//! iconforge iosify --input in.png --output out.ico [--base white|pick|custom] [--glyph-scale 0.76] [--no-shadow]
//! iconforge remask --input in.png --output out.ico
//! iconforge fetch  --query <name> [--pick <index>] --output out.ico [--cache-dir <dir>]
//! ```
//!
//! `fetch` prints the ranked CN+US candidate list; it applies the best
//! candidate only on an exact normalized match, otherwise it needs
//! `--pick <index>` (never silently use a wrong icon).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use image::RgbaImage;

use iconforge::composite::{composite, remask, Options};
use iconforge::gradient::{gradient_from_color, parse_custom, BaseTemplate, Rgb};
use iconforge::ico::save_ico;
use iconforge::itunes::{download_artwork, search_all};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        // Double-clicked from Explorer: native GUI window (Windows).
        // On non-Windows dev builds, fall back to the Web UI server.
        #[cfg(windows)]
        {
            if let Err(e) = iconforge::gui::run() {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
            return;
        }
        #[cfg(not(windows))]
        {
            serve(&HashMap::new());
            return;
        }
    }
    if let Err(e) = run(&args) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

/// Start the Web UI server and open the browser. On startup failure,
/// pause so a double-clicked console window doesn't flash away.
fn serve(flags: &HashMap<String, String>) -> ! {
    use std::io::IsTerminal;
    let port: u16 = flags.get("port").and_then(|s| s.parse().ok()).unwrap_or(0);
    match iconforge::server::run(port) {
        Ok(url) => {
            iconforge::server::open_browser(&url);
            // run() blocks serving; reaching here means the server stopped.
            std::process::exit(0);
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            if std::io::stdin().is_terminal() {
                println!("\n按回车退出……");
                let _ = std::io::stdin().read_line(&mut String::new());
            }
            std::process::exit(1);
        }
    }
}

fn run(args: &[String]) -> anyhow::Result<()> {
    let (cmd, flags) = parse(args);
    match cmd.as_str() {
        "serve" => {
            serve(&flags);
        }
        "gui" => {
            #[cfg(windows)]
            {
                let resume = flags
                    .get("resume-ui")
                    .map(|token| iconforge::elevation::ResumeState::decode(token))
                    .transpose()?;
                iconforge::gui::run_with_resume(resume)?;
            }
            #[cfg(not(windows))]
            {
                anyhow::bail!("native GUI is Windows-only; use `serve` on this platform");
            }
        }
        "base" => {
            let out = req(&flags, "out")?;
            let base = base_from(&flags)?;
            let size = flags
                .get("size")
                .and_then(|s| s.parse().ok())
                .unwrap_or(512);
            let plate = iconforge::gradient::render_base(size, &base);
            save_ico(&plate, Path::new(&out))?;
            println!("wrote {out}");
        }
        "iosify" => {
            let input = req(&flags, "input")?;
            let output = req(&flags, "output")?;
            let glyph_scale = flags
                .get("glyph-scale")
                .and_then(|s| s.parse().ok())
                .unwrap_or(iconforge::composite::DEFAULT_GLYPH_SCALE);
            let src = load_image(&input)?;
            // "pick" needs the source image: resolve it here, not in
            // base_from (which has no image to sample from).
            let base = if flags.get("base").map(|s| s.as_str()) == Some("pick") {
                iconforge::gradient::resolve_base("pick", &src).0
            } else {
                base_from(&flags)?
            };
            let opts = Options {
                base,
                glyph_scale,
                shadow: !flags.contains_key("no-shadow"),
                icon_radius: iconforge::composite::DEFAULT_ICON_RADIUS,
                base_size: iconforge::composite::DEFAULT_BASE_SIZE,
                base_exponent: iconforge::composite::DEFAULT_BASE_EXPONENT,
            };
            let out = composite(&src, &opts);
            save_ico(&out.image, Path::new(&output))?;
            println!(
                "wrote {output} (branch: {:?}, fg: {:.2})",
                out.branch, out.foreground_fraction
            );
        }
        "remask" => {
            let input = req(&flags, "input")?;
            let output = req(&flags, "output")?;
            let src = load_image(&input)?;
            let out = remask(&src);
            save_ico(&out, Path::new(&output))?;
            println!("wrote {output}");
        }
        "fetch" => {
            let query = req(&flags, "query")?;
            let output = req(&flags, "output")?;
            let cands = search_all(&query)?;
            if cands.is_empty() {
                anyhow::bail!("no iTunes candidates for {query:?} (tried CN+US, iOS+Mac)");
            }
            for (i, (score, c)) in cands.iter().enumerate() {
                println!(
                    "[{i}] score={score} {name} — {artist} (v{ver}, {countries}, {kind}, {bundle})",
                    name = c.track_name,
                    artist = c.artist_name,
                    ver = c.version,
                    countries = c.countries.join("+"),
                    kind = c.kind,
                    bundle = c.bundle_id,
                );
            }
            let chosen = if let Some(pick) = flags.get("pick") {
                let idx: usize = pick.parse()?;
                cands
                    .get(idx)
                    .ok_or_else(|| anyhow::anyhow!("--pick {idx} out of range"))?
            } else if cands[0].0 == 100 {
                &cands[0]
            } else {
                anyhow::bail!("no exact match; re-run with --pick <index> to choose a candidate");
            };
            // Knockoff warning: an exact store-title match can still be a
            // same-name copycat (verified: "PotPlayer" -> YANAPPS LAB LLC).
            // The developer + bundleId are printed above; eyeball them.
            println!(
                "核对：开发者 {}，bundleId {}，如非官方请用 --pick 换一个",
                chosen.1.artist_name, chosen.1.bundle_id
            );
            let art = download_artwork(&chosen.1)?;
            let out = remask(&art);
            save_ico(&out, Path::new(&output))?;
            if let Some(cache_dir) = flags.get("cache-dir") {
                let name = iconforge::itunes::cache_name(&chosen.1);
                let p = PathBuf::from(cache_dir).join(name);
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                art.save(&p)?;
                println!("cached artwork: {}", p.display());
            }
            println!("wrote {output} from {}", chosen.1.track_name);
        }
        "" | "help" | "--help" | "-h" => {
            print_help();
        }
        other => {
            anyhow::bail!("unknown subcommand {other:?}; try: base | iosify | remask | fetch");
        }
    }
    Ok(())
}

fn print_help() {
    println!("IconForge — 桌面图标 iOS 化（双击直接打开原生界面）");
    println!("  iconforge            启动完整版（原生窗口，Windows）");
    println!("  iconforge gui        同上");
    println!("  iconforge serve      Web 界面版 [--port 18731]（调试 / 备用）");
    println!("  iconforge base   --out base.ico [--base white|custom] [--size 512]");
    println!("  iconforge iosify --input in.png --output out.ico [--base white|pick|custom] [--glyph-scale 0.76] [--no-shadow]");
    println!("  iconforge remask --input in.png --output out.ico");
    println!(
        "  iconforge fetch  --query <name> [--pick <index>] --output out.ico [--cache-dir <dir>]"
    );
}

fn parse(args: &[String]) -> (String, HashMap<String, String>) {
    let mut cmd = String::new();
    let mut flags = HashMap::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.starts_with("--") {
            let key = a.trim_start_matches('-').to_string();
            if i + 1 < args.len() && !args[i + 1].starts_with("--") {
                flags.insert(key, args[i + 1].clone());
                i += 2;
            } else {
                flags.insert(key, String::new());
                i += 1;
            }
        } else if cmd.is_empty() {
            cmd = a.clone();
            i += 1;
        } else {
            i += 1;
        }
    }
    (cmd, flags)
}

fn req(flags: &HashMap<String, String>, key: &str) -> anyhow::Result<String> {
    flags
        .get(key)
        .cloned()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing --{key}"))
}

fn base_from(flags: &HashMap<String, String>) -> anyhow::Result<BaseTemplate> {
    match flags.get("base").map(|s| s.as_str()) {
        Some("blue") => Ok(BaseTemplate::BLUE), // legacy: UI no longer offers it
        Some("white") | None => Ok(BaseTemplate::WHITE),
        Some("pure") => Ok(BaseTemplate::PURE_WHITE),
        Some(s) if s.starts_with("custom:") => match parse_custom(s) {
            Some(c) => Ok(gradient_from_color(c)),
            None => anyhow::bail!("bad --base {s:?}; use custom:#RRGGBB"),
        },
        Some("pick") => {
            anyhow::bail!("取色需要输入图：用 iosify --base pick --input ...")
        }
        Some("custom") => {
            let top = Rgb::from_hex(req(flags, "top")?.as_str())?;
            let bottom = Rgb::from_hex(req(flags, "bottom")?.as_str())?;
            Ok(BaseTemplate::custom(top, bottom))
        }
        Some(other) => {
            anyhow::bail!("unknown --base {other:?}; use white|pure|pick|custom|custom:#RRGGBB")
        }
    }
}

fn load_image(path: &str) -> anyhow::Result<RgbaImage> {
    let img = image::open(path)?;
    Ok(img.to_rgba8())
}
