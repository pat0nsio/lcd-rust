// SPDX-License-Identifier: GPL-3.0-or-later
//! System monitor for Turing Smart Screen rev. A panels.

mod config;
mod metrics;
mod render;
mod spotify;
mod stats;
mod theme;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use turing_lcd::canvas::Canvas;
use turing_lcd::{detect_port, Display};

use config::Config;
use metrics::Metrics;
use render::Renderer;
use theme::Theme;

/// Theme used when neither the config nor `--theme` names one.
const BUILTIN_THEME: &str = include_str!("../../themes/default.toml");

static RUNNING: AtomicBool = AtomicBool::new(true);
/// Raised by SIGUSR1: the next frame switches to the other theme.
static TOGGLE: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_sig: libc::c_int) {
    RUNNING.store(false, Ordering::SeqCst);
}

extern "C" fn on_toggle(_sig: libc::c_int) {
    TOGGLE.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    // SAFETY: the handlers only store into an AtomicBool, which is
    // async-signal-safe.
    unsafe {
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGUSR1, on_toggle as *const () as libc::sighandler_t);
    }
}

const USAGE: &str = "\
turing-monitor - system monitor for Turing Smart Screen rev. A

USAGE:
    turing-monitor [OPTIONS] [COMMAND]

COMMANDS:
    run                 Run the monitor (default)
    detect              Print the detected serial port and panel model
    clear               Blank the panel to white and exit
    on | off            Turn the backlight on or off
    brightness <0-100>  Set brightness and exit
    reset               Reboot the panel (the port disappears briefly)

OPTIONS:
    -c, --config <FILE>     Config file (default: ~/.config/turing-monitor.conf)
    -t, --theme <FILE>      Theme file (default: the one built in)
        --theme-alt <FILE>  Second theme; SIGUSR1 alternates between the two
        --check-theme       Load the theme, report problems, and exit
        --list-metrics      Print every metric a theme can reference
    -p, --port <PATH>       Serial port, e.g. /dev/ttyACM0 (default: autodetect)
    -b, --brightness <N>    Backlight 0-100
    -r, --refresh <MS>      Refresh interval in milliseconds
        --once              Draw a single frame and exit
        --stats             Log bytes sent per frame to stderr
        --dump <FILE.ppm>   Render to a PPM file without touching hardware
    -h, --help              Show this help
";

fn main() {
    // Rust ignores SIGPIPE, which turns `turing-monitor --list-metrics | head`
    // into a panic instead of a clean exit. Restore the default.
    // SAFETY: setting a disposition to SIG_DFL before any threads exist.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    if let Err(e) = run() {
        eprintln!("turing-monitor: {e}");
        std::process::exit(1);
    }
}

struct Args {
    command: String,
    config: Option<String>,
    port: Option<String>,
    brightness: Option<u8>,
    refresh: Option<u64>,
    once: bool,
    show_stats: bool,
    dump: Option<String>,
    theme: Option<String>,
    theme_alt: Option<String>,
    check_theme: bool,
    list_metrics: bool,
    value: Option<String>,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut a = Args {
        command: "run".into(),
        config: None,
        port: None,
        brightness: None,
        refresh: None,
        once: false,
        show_stats: false,
        dump: None,
        theme: None,
        theme_alt: None,
        check_theme: false,
        list_metrics: false,
        value: None,
    };
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < argv.len() {
        let arg = argv[i].as_str();
        let mut next = |what: &str| -> Result<String, String> {
            i += 1;
            argv.get(i)
                .cloned()
                .ok_or_else(|| format!("{what} needs a value"))
        };
        match arg {
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "-c" | "--config" => a.config = Some(next(arg)?),
            "-p" | "--port" => a.port = Some(next(arg)?),
            "-b" | "--brightness" => {
                a.brightness = Some(next(arg)?.parse().map_err(|_| "brightness must be 0-100")?)
            }
            "-r" | "--refresh" => {
                a.refresh = Some(next(arg)?.parse().map_err(|_| "refresh must be a number")?)
            }
            "--once" => a.once = true,
            "--stats" => a.show_stats = true,
            "--dump" => a.dump = Some(next(arg)?),
            "-t" | "--theme" => a.theme = Some(next(arg)?),
            "--theme-alt" => a.theme_alt = Some(next(arg)?),
            "--check-theme" => a.check_theme = true,
            "--list-metrics" => a.list_metrics = true,
            "run" | "detect" | "clear" | "on" | "off" | "reset" | "brightness" => {
                a.command = arg.to_string()
            }
            other if a.command == "brightness" && a.value.is_none() => {
                a.value = Some(other.to_string())
            }
            other => return Err(format!("unknown argument `{other}` (try --help)")),
        }
        i += 1;
    }
    Ok(Some(a))
}

fn default_config_path() -> Option<String> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{h}/.config")))?;
    Some(format!("{base}/turing-monitor.conf"))
}

fn run() -> Result<(), String> {
    let Some(args) = parse_args()? else {
        return Ok(());
    };

    // An explicit --config must exist; the default path is optional.
    let mut cfg = match &args.config {
        Some(p) => Config::load(p)?,
        None => match default_config_path().filter(|p| std::path::Path::new(p).is_file()) {
            Some(p) => Config::load(&p)?,
            None => Config::default(),
        },
    };
    if let Some(p) = args.port.clone() {
        cfg.port = Some(p);
    }
    if let Some(b) = args.brightness {
        cfg.brightness = b.min(100);
    }
    if let Some(r) = args.refresh {
        cfg.refresh_ms = r.max(100);
    }
    if let Some(t) = args.theme_alt.clone() {
        cfg.theme_alt = Some(t);
    }

    if args.list_metrics {
        return cmd_list_metrics(&cfg);
    }
    if args.check_theme {
        return cmd_check_theme(&cfg, args.theme.as_deref());
    }

    match args.command.as_str() {
        "detect" => cmd_detect(&cfg),
        "clear" => with_display(&cfg, |d| d.device_mut().clear()),
        "on" => with_display(&cfg, |d| d.device_mut().screen_on()),
        "off" => with_display(&cfg, |d| d.device_mut().screen_off()),
        "reset" => with_display(&cfg, |d| d.device_mut().reset()),
        "brightness" => {
            let level: u8 = args
                .value
                .as_deref()
                .ok_or("brightness needs a level, e.g. `brightness 40`")?
                .parse()
                .map_err(|_| "brightness must be 0-100")?;
            with_display(&cfg, |d| d.device_mut().set_brightness(level))
        }
        _ => match args.dump.clone() {
            Some(path) => cmd_dump(&cfg, &args, &path),
            None => cmd_run(&cfg, &args),
        },
    }
}

/// Canvas size for the offline commands, which never open the panel. The
/// rev. A panel is 320x480 upright, so a rotated theme is laid out the other
/// way round.
fn canvas_size(orientation: turing_lcd::Orientation) -> (u16, u16) {
    use turing_lcd::Orientation::*;
    match orientation {
        Portrait | ReversePortrait => (320, 480),
        Landscape | ReverseLandscape => (480, 320),
    }
}

/// Everything `--check-theme` and `--dump` need to lay a theme out without
/// the panel plugged in.
struct Offline {
    canvas: Canvas,
    renderer: Renderer,
    sampler: Sampler,
}

impl Offline {
    fn new(cfg: &Config, background: turing_lcd::Rgb) -> Result<Offline, String> {
        let (w, h) = canvas_size(cfg.orientation);
        Ok(Offline {
            canvas: Canvas::new(w, h, background),
            renderer: Renderer::new(cfg.font_regular.as_deref(), cfg.font_bold.as_deref(), w)?,
            sampler: Sampler::new(cfg),
        })
    }

    fn size(&self) -> (u16, u16) {
        (self.canvas.width(), self.canvas.height())
    }

    /// Sample and draw one frame, handing back the metrics it used so a
    /// caller can report on them without sampling a second time.
    fn frame(&mut self, theme: &Theme) -> (render::RenderReport, Metrics) {
        let m = self.sampler.sample();
        let report = self.renderer.render(&mut self.canvas, theme, &m);
        (report, m)
    }
}

fn with_display<F>(cfg: &Config, f: F) -> Result<(), String>
where
    F: FnOnce(&mut Display) -> Result<(), String>,
{
    let mut d = Display::open(cfg.port.as_deref(), cfg.orientation)?;
    f(&mut d)
}

fn cmd_detect(cfg: &Config) -> Result<(), String> {
    match detect_port() {
        Some(p) => println!("port:      {p}"),
        None => println!("port:      not found"),
    }
    let d = Display::open(cfg.port.as_deref(), cfg.orientation)?;
    println!("opened:    {}", d.device().path());
    println!("model:     {:?}", d.device().sub_revision());
    println!("size:      {}x{}", d.width(), d.height());
    println!("rotation:  {:?}", d.device().orientation());
    Ok(())
}

/// Reads every metric a theme can reference.
struct Sampler {
    cpu: stats::CpuSampler,
    mem: stats::MemSampler,
    net: stats::NetSampler,
    gpu: Option<stats::AmdGpu>,
    cpu_temp: Option<stats::TempSensor>,
    hostname: String,
    cpu_model: String,
    disk_mount: String,
    /// Only ask the player when some theme in play actually shows it: it is
    /// the one metric that costs a process instead of a file read.
    want_spotify: bool,
    /// Caratula en curso y la URL de la que salio, para bajarla una vez por
    /// cancion y no una vez por fotograma.
    art_url: String,
    art: Option<std::rc::Rc<turing_lcd::Image>>,
}

impl Sampler {
    fn new(cfg: &Config) -> Sampler {
        Sampler {
            cpu: stats::CpuSampler::new(),
            mem: stats::MemSampler::new(),
            net: stats::NetSampler::new(cfg.net_interface.as_deref()),
            gpu: stats::AmdGpu::find(),
            cpu_temp: stats::TempSensor::find(&cfg.cpu_temp_chip, cfg.cpu_temp_label.as_deref())
                // Fall back to any sensor on that chip if the label moved.
                .or_else(|| stats::TempSensor::find(&cfg.cpu_temp_chip, None)),
            hostname: stats::hostname(),
            cpu_model: stats::cpu_model(),
            disk_mount: cfg.disk.clone(),
            want_spotify: false,
            art_url: String::new(),
            art: None,
        }
    }

    fn sample(&mut self) -> Metrics {
        self.cpu.sample();
        self.mem.sample();
        self.net.sample();

        let mut m = Metrics::new();

        let (l1, l5, l15) = stats::load_average();
        m.set_text("sys.hostname", self.hostname.clone());
        m.set_num("sys.uptime", stats::uptime_secs() as f64);
        m.set_text("sys.load", format!("{l1:.2} {l5:.2} {l15:.2}"));
        // Solo la de 1 minuto por separado: es la unica que sirve para una
        // barra o una rampa. Las tres juntas ya van en sys.load como texto.
        m.set_num("sys.load1", l1 as f64);

        m.set_text("cpu.model", self.cpu_model.clone());
        m.set_num("cpu.usage", self.cpu.usage as f64);
        m.set_series("cpu.cores", self.cpu.cores.clone());
        m.set_opt_num("cpu.freq", self.cpu.frequency_mhz().map(|f| f as f64));
        if let Some(t) = self.cpu_temp.as_ref().and_then(|s| s.read()) {
            m.set_num("cpu.temp", t as f64);
            // Normalised copy so colour ramps, which work on 0..=1, can use it.
            m.set_num("cpu.temp_ratio", (t / 100.0).clamp(0.0, 1.0) as f64);
        }

        if let Some(gpu) = &self.gpu {
            m.set_num("gpu.usage", gpu.usage() as f64);
            if let Some(t) = gpu.temp.as_ref().and_then(|s| s.read()) {
                m.set_num("gpu.temp", t as f64);
                m.set_num("gpu.temp_ratio", (t / 100.0).clamp(0.0, 1.0) as f64);
            }
            if let Some((used, total)) = gpu.vram() {
                m.set_num("gpu.vram_used", used as f64);
                m.set_num("gpu.vram_total", total as f64);
                if total > 0 {
                    m.set_num("gpu.vram_usage", used as f64 / total as f64);
                }
            }
        }

        let mem = self.mem.mem;
        m.set_num("mem.usage", mem.usage() as f64);
        m.set_num("mem.used", mem.used_kb() as f64 * 1024.0);
        m.set_num("mem.total", mem.total_kb as f64 * 1024.0);
        m.set_num("mem.available", mem.available_kb as f64 * 1024.0);
        // Swap keys stay undefined without swap, so `require` drops the section.
        if mem.swap_total_kb > 0 {
            m.set_num("swap.usage", mem.swap_usage() as f64);
            m.set_num("swap.used", mem.swap_used_kb() as f64 * 1024.0);
            m.set_num("swap.total", mem.swap_total_kb as f64 * 1024.0);
        }

        let disk = stats::disk_usage(&self.disk_mount);
        m.set_text("disk.mount", self.disk_mount.clone());
        m.set_num("disk.usage", disk.usage() as f64);
        m.set_num("disk.used", disk.used() as f64);
        m.set_num("disk.free", disk.free as f64);
        m.set_num("disk.total", disk.total as f64);

        m.set_text("net.iface", self.net.interface().to_string());
        m.set_num("net.rx", self.net.rx_rate);
        m.set_num("net.tx", self.net.tx_rate);

        if self.want_spotify {
            self.sample_spotify(&mut m);
        }

        m
    }

    /// `spotify.status` existe siempre que se mire el reproductor, para que un
    /// tema pueda decir "no suena nada". El resto de claves solo aparecen con
    /// pista, asi que `require` las descarta sin dejar hueco.
    fn sample_spotify(&mut self, m: &mut Metrics) {
        let Some(np) = spotify::poll() else {
            m.set_text("spotify.status", "sin reproductor");
            self.art_url.clear();
            self.art = None;
            return;
        };
        // Solo al cambiar de pista: descargar y descodificar cuesta, dibujarla
        // no. Si la descarga falla no se reintenta hasta la siguiente cancion.
        if np.art_url != self.art_url {
            self.art_url = np.art_url.clone();
            self.art = spotify::art(&np.art_url).map(std::rc::Rc::new);
        }
        if let Some(art) = &self.art {
            m.set_image("spotify.art", art.clone());
        }
        m.set_text(
            "spotify.status",
            match np.status.as_str() {
                "Playing" => "sonando",
                "Paused" => "en pausa",
                _ => "parado",
            },
        );
        for (key, value) in [
            ("spotify.title", np.title),
            ("spotify.artist", np.artist),
            ("spotify.album", np.album),
        ] {
            if !value.is_empty() {
                m.set_text(key, value);
            }
        }
        // 1/0 para que un tema pueda encender algo solo mientras suena.
        m.set_num("spotify.playing", (np.status == "Playing") as u8 as f64);
        m.set_num("spotify.position", np.position);
        // Sin duracion no hay barra: los anuncios no la publican.
        if np.length > 0.0 {
            m.set_num("spotify.length", np.length);
            m.set_num("spotify.progress", (np.position / np.length).clamp(0.0, 1.0));
        }
    }
}

/// `--theme` wins over the config file; otherwise the built-in theme is used.
fn load_theme(cfg: &Config, arg: Option<&str>) -> Result<Theme, String> {
    match arg.or(cfg.theme.as_deref()) {
        Some(path) => Theme::load(path),
        None => Theme::parse(BUILTIN_THEME).map_err(|e| format!("built-in theme: {e}")),
    }
}

/// Todos los temas que el proceso puede mostrar: el principal y, si la
/// configuracion lo nombra, el alterno. SIGUSR1 va rotando por la lista.
fn load_themes(cfg: &Config, arg: Option<&str>) -> Result<Vec<Theme>, String> {
    let mut themes = vec![load_theme(cfg, arg)?];
    if let Some(path) = &cfg.theme_alt {
        themes.push(Theme::load(path)?);
    }
    Ok(themes)
}

/// Whether a theme shows anything from the player, which is what decides if
/// the sampler bothers asking for it.
fn uses_spotify(theme: &Theme) -> bool {
    render::referenced_metrics(theme)
        .iter()
        .any(|k| k.starts_with("spotify."))
}

fn cmd_check_theme(cfg: &Config, arg: Option<&str>) -> Result<(), String> {
    let theme = load_theme(cfg, arg)?;
    let source = arg.or(cfg.theme.as_deref()).unwrap_or("<built-in>");
    println!("theme:     {} ({})", theme.name, source);
    if let Some(author) = &theme.author {
        println!("author:    {author}");
    }
    println!("sections:  {}", theme.sections.len());
    println!("margin:    {}", theme.margin);
    println!("columns:   {} (gap {})", theme.columns, theme.column_gap);

    // Lay the theme out on a real canvas so we can report sections that
    // silently would not fit, and say which referenced metrics exist here.
    let mut off = Offline::new(cfg, theme.background)?;
    off.sampler.want_spotify = uses_spotify(&theme);
    let (w, h) = off.size();
    let (report, m) = off.frame(&theme);
    let refs = render::referenced_metrics(&theme);
    let missing: Vec<&String> = refs.iter().filter(|k| !m.has(k)).collect();
    println!(
        "layout:    {} of {} px used at {w}x{h}",
        report.used_height, report.available_height
    );
    warn_skipped(&report);

    println!("metrics:   {} referenced", refs.len());
    if missing.is_empty() {
        println!("           all available on this machine");
    } else {
        println!("           not available here: {}",
            missing.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "));
        println!("           (sections with a matching `require` are skipped, not broken)");
    }
    Ok(())
}

/// Sections dropped for lack of room are a theme bug, not a runtime error,
/// so say so loudly rather than leaving a blank patch on the panel.
fn warn_skipped(report: &render::RenderReport) {
    if report.skipped.is_empty() {
        return;
    }
    eprintln!(
        "warning: {} section(s) did not fit and were not drawn: {}",
        report.skipped.len(),
        report.skipped.join(", ")
    );
    eprintln!("         reduce row heights or gaps, or drop a section");
}

fn cmd_list_metrics(cfg: &Config) -> Result<(), String> {
    let mut sampler = Sampler::new(cfg);
    // Nadie ha pedido un tema aqui, asi que se listan todas las metricas.
    sampler.want_spotify = true;
    // Two passes: rates and CPU load need a delta to be non-zero.
    let _ = sampler.sample();
    std::thread::sleep(Duration::from_millis(300));
    let m = sampler.sample();

    println!("{:<20} {:<8} {}", "METRIC", "KIND", "CURRENT");
    for (key, value) in m.keys() {
        let (kind, shown) = match value {
            metrics::Value::Num(v) => ("num", format!("{v:.4}")),
            metrics::Value::Text(t) => ("text", t.clone()),
            metrics::Value::Series(s) => ("series", format!("{} values", s.len())),
            metrics::Value::Image(i) => ("image", format!("{}x{} px", i.w, i.h)),
        };
        println!("{key:<20} {kind:<8} {shown}");
    }
    Ok(())
}

fn cmd_run(cfg: &Config, args: &Args) -> Result<(), String> {
    install_signal_handlers();

    let themes = load_themes(cfg, args.theme.as_deref())?;
    let mut current = 0usize;
    let mut display = Display::open(cfg.port.as_deref(), cfg.orientation)?;
    display.device_mut().set_brightness(cfg.brightness)?;
    display.device_mut().screen_on()?;

    let mut sampler = Sampler::new(cfg);
    // Por tema, no por proceso: con la hoja de metricas delante no se le
    // pregunta nada al reproductor.
    let wants_spotify: Vec<bool> = themes.iter().map(uses_spotify).collect();
    let mut renderer = Renderer::new(
        cfg.font_regular.as_deref(),
        cfg.font_bold.as_deref(),
        display.width(),
    )?;
    let interval = Duration::from_millis(cfg.refresh_ms);

    while RUNNING.load(Ordering::SeqCst) {
        let started = Instant::now();
        if TOGGLE.swap(false, Ordering::SeqCst) {
            current = (current + 1) % themes.len();
            // Otro tema es otro fondo: se reenvia el fotograma entero.
            display.invalidate();
        }
        sampler.want_spotify = wants_spotify[current];
        let m = sampler.sample();
        let _ = renderer.render(display.canvas(), &themes[current], &m);

        match display.flush() {
            Ok(s) if args.show_stats => eprintln!(
                "frame: {} rect(s), {} px, {} bytes, {:.1} ms",
                s.rects,
                s.pixels,
                s.bytes,
                started.elapsed().as_secs_f32() * 1000.0
            ),
            Ok(_) => {}
            Err(e) => {
                eprintln!("turing-monitor: {e}; reconnecting...");
                std::thread::sleep(Duration::from_secs(2));
                match Display::open(cfg.port.as_deref(), cfg.orientation) {
                    Ok(d) => {
                        display = d;
                        let _ = display.device_mut().set_brightness(cfg.brightness);
                        display.invalidate();
                    }
                    Err(e) => eprintln!("turing-monitor: reconnect failed: {e}"),
                }
                continue;
            }
        }

        if args.once {
            return Ok(());
        }
        // Sleep in short slices so Ctrl-C is picked up promptly.
        // El mismo troceado atiende al cambio de tema, para que la tecla no
        // tarde un refresco entero en notarse.
        let deadline = started + interval;
        while RUNNING.load(Ordering::SeqCst) && !TOGGLE.load(Ordering::SeqCst) && Instant::now() < deadline
        {
            let left = deadline.saturating_duration_since(Instant::now());
            std::thread::sleep(left.min(Duration::from_millis(100)));
        }
    }

    if cfg.screen_off_on_exit {
        let _ = display.device_mut().screen_off();
    }
    Ok(())
}

/// Render frames into memory and write the last one as a PPM. Useful for
/// working on a theme without the panel plugged in.
fn cmd_dump(cfg: &Config, args: &Args, path: &str) -> Result<(), String> {
    let theme = load_theme(cfg, args.theme.as_deref())?;
    let mut off = Offline::new(cfg, theme.background)?;
    off.sampler.want_spotify = uses_spotify(&theme);
    let (w, h) = off.size();

    // A few quick passes so the history plots have something in them.
    let mut report = render::RenderReport::default();
    for _ in 0..60 {
        report = off.frame(&theme).0;
        std::thread::sleep(Duration::from_millis(20));
    }

    let mut out = format!("P6\n{w} {h}\n255\n").into_bytes();
    out.extend_from_slice(off.canvas.bytes());
    std::fs::write(path, out).map_err(|e| format!("cannot write {path}: {e}"))?;
    println!("wrote {path} ({w}x{h}, theme `{}`)", theme.name);
    warn_skipped(&report);
    Ok(())
}
