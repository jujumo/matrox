//! matrox — Matrix digital rain whose intensity follows CPU/GPU load.

mod load;

use std::io::{self, BufWriter, Write};
use std::time::{Duration, Instant};

use clap::{Parser, ValueEnum};
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    queue,
    style::{Attribute, Color, Print, SetAttribute, SetBackgroundColor, SetForegroundColor},
    terminal::{self, ClearType},
};

#[derive(Parser, Debug)]
#[command(version, about = "Matrix digital rain in your terminal, driven by CPU/GPU activity")]
struct Args {
    /// Which load drives the rain
    #[arg(short, long, value_enum, default_value_t = Source::Max)]
    source: Source,

    /// Rain color
    #[arg(short, long, value_enum, default_value_t = Palette::Green)]
    color: Palette,

    /// Shift color towards red as load increases
    #[arg(short = 'H', long)]
    heat: bool,

    /// Character set
    #[arg(short = 'C', long, value_enum, default_value_t = Charset::Matrix)]
    charset: Charset,

    /// Color mode (auto uses truecolor when $COLORTERM says so, else 256 colors)
    #[arg(long, value_enum, default_value_t = ColorMode::Auto)]
    colors: ColorMode,

    /// Show the load HUD on the top line (toggle with 'h')
    #[arg(short = 'u', long)]
    hud: bool,

    /// Density multiplier (adjust live with +/-)
    #[arg(short, long, default_value_t = 1.0)]
    intensity: f32,

    /// Speed multiplier
    #[arg(long, default_value_t = 1.0)]
    speed: f32,

    /// Frames per second
    #[arg(short, long, default_value_t = 30)]
    fps: u32,

    /// Load smoothing time constant, in seconds
    #[arg(long, default_value_t = 1.0)]
    smooth: f32,

    /// Load sampling interval, in milliseconds
    #[arg(long, default_value_t = 500)]
    sample_ms: u64,

    /// Ignore real load and use this fixed value (0-100), for testing
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
    fixed: Option<u8>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Source {
    Cpu,
    Gpu,
    /// Max of CPU and GPU
    Max,
    /// Average of CPU and GPU
    Avg,
}

impl Source {
    fn next(self) -> Self {
        match self {
            Source::Cpu => Source::Gpu,
            Source::Gpu => Source::Max,
            Source::Max => Source::Avg,
            Source::Avg => Source::Cpu,
        }
    }

    fn pick(self, cpu: f32, gpu: Option<f32>) -> f32 {
        match (self, gpu) {
            (Source::Cpu, _) | (_, None) => cpu,
            (Source::Gpu, Some(g)) => g,
            (Source::Max, Some(g)) => cpu.max(g),
            (Source::Avg, Some(g)) => (cpu + g) * 0.5,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Palette {
    Green,
    Red,
    Blue,
    Cyan,
    Yellow,
    Magenta,
    White,
    Rainbow,
}

impl Palette {
    const ALL: [Palette; 8] = [
        Palette::Green,
        Palette::Red,
        Palette::Blue,
        Palette::Cyan,
        Palette::Yellow,
        Palette::Magenta,
        Palette::White,
        Palette::Rainbow,
    ];

    fn next(self) -> Self {
        let i = Self::ALL.iter().position(|&p| p == self).unwrap();
        Self::ALL[(i + 1) % Self::ALL.len()]
    }

    /// (hue in degrees, saturation)
    fn hue_sat(self) -> (f32, f32) {
        match self {
            Palette::Green | Palette::Rainbow => (120.0, 1.0),
            Palette::Red => (0.0, 1.0),
            Palette::Blue => (220.0, 1.0),
            Palette::Cyan => (180.0, 1.0),
            Palette::Yellow => (55.0, 1.0),
            Palette::Magenta => (300.0, 1.0),
            Palette::White => (0.0, 0.0),
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Charset {
    /// Half-width katakana, digits and a few symbols
    Matrix,
    /// Half-width katakana only
    Katakana,
    /// Printable ASCII
    Ascii,
    /// 0 and 1
    Binary,
}

impl Charset {
    fn glyphs(self) -> Vec<char> {
        let kana = || ('\u{FF66}'..='\u{FF9D}').collect::<Vec<_>>();
        match self {
            Charset::Matrix => {
                let mut v = kana();
                v.extend('0'..='9');
                v.extend(":.\"=*+-<>|¦_".chars());
                v
            }
            Charset::Katakana => kana(),
            Charset::Ascii => ('!'..='~').collect(),
            Charset::Binary => vec!['0', '1'],
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum ColorMode {
    Auto,
    Truecolor,
    #[value(name = "256")]
    Ansi256,
}

// ---------------------------------------------------------------------------
// Simulation

#[derive(Copy, Clone, Default)]
struct Cell {
    ch: char,
    /// 1.0 when freshly written, fades to 0.0
    life: f32,
    /// life lost per second
    decay: f32,
    head: bool,
}

struct Drop {
    x: u16,
    y: f32,
    row: i32,
    speed: f32,
    len: f32,
}

struct Rain {
    w: u16,
    h: u16,
    cells: Vec<Cell>,
    drops: Vec<Drop>,
    glyphs: Vec<char>,
    col_top: Vec<f32>,
}

impl Rain {
    fn new(w: u16, h: u16, glyphs: Vec<char>) -> Self {
        Self {
            w,
            h,
            cells: vec![Cell::default(); w as usize * h as usize],
            drops: Vec::new(),
            glyphs,
            col_top: vec![f32::INFINITY; w as usize],
        }
    }

    fn glyph(&self) -> char {
        self.glyphs[fastrand::usize(..self.glyphs.len())]
    }

    fn idx(&self, x: u16, y: u16) -> usize {
        y as usize * self.w as usize + x as usize
    }

    /// Advance by `dt` seconds. `load` in [0,1] drives density, speed and glitching.
    fn step(&mut self, dt: f32, load: f32, intensity: f32, speed_mul: f32) {
        // Fade trails.
        for c in &mut self.cells {
            if c.life > 0.0 && !c.head {
                c.life = (c.life - c.decay * dt).max(0.0);
            }
        }

        let l = load.clamp(0.0, 1.0);
        // Drops spawned per column per second: a light drizzle when idle, a storm at 100%.
        let rate = (0.012 + 0.55 * l.powf(0.8)) * intensity;
        let mul = (0.35 + 1.5 * l) * speed_mul;

        // Spawn new drops, avoiding stacking on a drop that just entered the column.
        self.col_top.fill(f32::INFINITY);
        for d in &self.drops {
            let t = &mut self.col_top[d.x as usize];
            *t = t.min(d.y);
        }
        for x in 0..self.w {
            if self.col_top[x as usize] > 2.0 && fastrand::f32() < rate * dt {
                self.drops.push(Drop {
                    x,
                    y: 0.0,
                    row: -1,
                    speed: 6.0 + fastrand::f32() * 18.0,
                    len: 5.0 + fastrand::f32() * (self.h as f32 * 0.8).max(10.0),
                });
            }
        }

        // Move drops, writing fresh glyphs on every row they cross.
        let h = self.h;
        let mut drops = std::mem::take(&mut self.drops);
        for d in &mut drops {
            let v = d.speed * mul;
            d.y += v * dt;
            let target = d.y.floor() as i32;
            while d.row < target {
                if d.row >= 0 && d.row < h as i32 {
                    let i = self.idx(d.x, d.row as u16);
                    self.cells[i].head = false;
                }
                d.row += 1;
                if d.row < h as i32 {
                    let ch = self.glyph();
                    let i = self.idx(d.x, d.row as u16);
                    self.cells[i] = Cell { ch, life: 1.0, decay: v / d.len, head: true };
                }
            }
            // Heads flicker through glyphs.
            if d.row >= 0 && d.row < h as i32 && fastrand::f32() < 0.5 {
                let ch = self.glyph();
                let i = self.idx(d.x, d.row as u16);
                self.cells[i].ch = ch;
            }
        }
        drops.retain(|d| d.row < h as i32);
        self.drops = drops;

        // Random glyph mutations in trails; more under load.
        let p = (0.04 + 0.6 * l) * dt;
        let n = (self.cells.len() as f32 * p) as usize + (fastrand::f32() < p.fract()) as usize;
        for _ in 0..n {
            let i = fastrand::usize(..self.cells.len());
            if self.cells[i].life > 0.0 {
                self.cells[i].ch = self.glyph();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering

#[derive(Copy, Clone, PartialEq)]
struct Glyph {
    ch: char,
    fg: Color,
    bg: Color,
    bold: bool,
}

const BLANK: Glyph = Glyph { ch: ' ', fg: Color::Reset, bg: Color::Reset, bold: false };
const LEVELS: f32 = 24.0;

fn hsv(h: f32, s: f32, v: f32) -> (f32, f32, f32) {
    let h = h.rem_euclid(360.0) / 60.0;
    let c = v * s;
    let x = c * (1.0 - (h % 2.0 - 1.0).abs());
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    (r + m, g + m, b + m)
}

struct Painter {
    truecolor: bool,
}

impl Painter {
    fn color(&self, (r, g, b): (f32, f32, f32)) -> Color {
        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
        if self.truecolor {
            Color::Rgb { r: q(r), g: q(g), b: q(b) }
        } else {
            let c = |v: f32| (v.clamp(0.0, 1.0) * 5.0).round() as u8;
            Color::AnsiValue(16 + 36 * c(r) + 6 * c(g) + c(b))
        }
    }

    /// Color for a cell with remaining `life`, at the given hue.
    fn shade(&self, hue: f32, sat: f32, life: f32, head: bool) -> (Color, bool) {
        if head {
            let (r, g, b) = hsv(hue, sat * 0.25, 1.0);
            return (self.color((r, g, b)), true);
        }
        let t = (life * LEVELS).ceil() / LEVELS;
        let v = 0.12 + 0.88 * t.powf(1.6);
        // The freshest part of the trail glows slightly towards white.
        let s = sat * (1.0 - 0.45 * ((t - 0.85) / 0.15).clamp(0.0, 1.0));
        (self.color(hsv(hue, s, v)), t > 0.6)
    }
}

struct Screen<W: Write> {
    out: W,
    front: Vec<Glyph>,
    w: u16,
    h: u16,
}

impl<W: Write> Screen<W> {
    fn new(out: W, w: u16, h: u16) -> Self {
        Self { out, front: vec![BLANK; w as usize * h as usize], w, h }
    }

    fn resize(&mut self, w: u16, h: u16) -> io::Result<()> {
        self.w = w;
        self.h = h;
        self.front = vec![BLANK; w as usize * h as usize];
        queue!(self.out, SetAttribute(Attribute::Reset), terminal::Clear(ClearType::All))
    }

    /// Emit only the cells that differ from what is already on screen.
    fn present(&mut self, back: &[Glyph]) -> io::Result<()> {
        let (mut fg, mut bg, mut bold) = (None, None, None);
        let mut cursor_at: Option<(u16, u16)> = None;
        for y in 0..self.h {
            for x in 0..self.w {
                let i = y as usize * self.w as usize + x as usize;
                let g = back[i];
                if self.front[i] == g {
                    continue;
                }
                self.front[i] = g;
                if cursor_at != Some((x, y)) {
                    queue!(self.out, cursor::MoveTo(x, y))?;
                }
                if bold != Some(g.bold) {
                    let a = if g.bold { Attribute::Bold } else { Attribute::NormalIntensity };
                    queue!(self.out, SetAttribute(a))?;
                    bold = Some(g.bold);
                }
                if fg != Some(g.fg) {
                    queue!(self.out, SetForegroundColor(g.fg))?;
                    fg = Some(g.fg);
                }
                if bg != Some(g.bg) {
                    queue!(self.out, SetBackgroundColor(g.bg))?;
                    bg = Some(g.bg);
                }
                queue!(self.out, Print(g.ch))?;
                cursor_at = Some((x + 1, y));
            }
        }
        self.out.flush()
    }
}

/// Restores the terminal on drop, including on early return.
struct TermGuard;

impl TermGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut out = io::stdout();
        queue!(out, terminal::EnterAlternateScreen, cursor::Hide, terminal::Clear(ClearType::All))?;
        out.flush()?;
        Ok(TermGuard)
    }
}

impl std::ops::Drop for TermGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

fn restore_terminal() {
    let mut out = io::stdout();
    let _ = queue!(out, SetAttribute(Attribute::Reset), cursor::Show, terminal::LeaveAlternateScreen);
    let _ = out.flush();
    let _ = terminal::disable_raw_mode();
}

fn bar(v: f32, n: usize) -> String {
    let k = ((v.clamp(0.0, 1.0) * n as f32).round()) as usize;
    format!("{}{}", "█".repeat(k), "░".repeat(n - k))
}

// ---------------------------------------------------------------------------

fn main() -> io::Result<()> {
    let args = Args::parse();

    let truecolor = match args.colors {
        ColorMode::Truecolor => true,
        ColorMode::Ansi256 => false,
        ColorMode::Auto => std::env::var("COLORTERM")
            .map(|v| v.contains("truecolor") || v.contains("24bit"))
            .unwrap_or(false),
    };
    let painter = Painter { truecolor };

    let monitor = load::spawn_monitor(Duration::from_millis(args.sample_ms));

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        default_hook(info);
    }));

    let _guard = TermGuard::enter()?;
    let (mut w, mut h) = terminal::size()?;
    let mut rain = Rain::new(w, h, args.charset.glyphs());
    let mut screen = Screen::new(BufWriter::with_capacity(1 << 16, io::stdout()), w, h);
    let mut back = vec![BLANK; w as usize * h as usize];

    let mut source = args.source;
    let mut palette = args.color;
    let mut heat = args.heat;
    let mut hud = args.hud;
    let mut intensity = args.intensity.max(0.0);
    let mut paused = false;

    let frame = Duration::from_secs_f64(1.0 / args.fps.clamp(1, 240) as f64);
    let start = Instant::now();
    let mut last = Instant::now();
    let mut smoothed = 0.0f32;
    let mut next_frame = Instant::now();

    'main: loop {
        // Input, while waiting for the next frame.
        let mut timeout = next_frame.saturating_duration_since(Instant::now());
        while event::poll(timeout)? {
            match event::read()? {
                Event::Key(KeyEvent { code, modifiers, kind: KeyEventKind::Press, .. }) => match code {
                    KeyCode::Char('q') | KeyCode::Esc => break 'main,
                    KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => break 'main,
                    KeyCode::Char('h') => hud = !hud,
                    KeyCode::Char('c') => palette = palette.next(),
                    KeyCode::Char('t') => heat = !heat,
                    KeyCode::Char('s') => source = source.next(),
                    KeyCode::Char('+') | KeyCode::Char('=') => intensity = (intensity * 1.25).min(20.0),
                    KeyCode::Char('-') => intensity = (intensity / 1.25).max(0.05),
                    KeyCode::Char(' ') | KeyCode::Char('p') => paused = !paused,
                    _ => {}
                },
                Event::Resize(nw, nh) => {
                    (w, h) = (nw, nh);
                    rain = Rain::new(w, h, args.charset.glyphs());
                    back = vec![BLANK; w as usize * h as usize];
                    screen.resize(w, h)?;
                }
                _ => {}
            }
            timeout = next_frame.saturating_duration_since(Instant::now());
        }
        next_frame += frame;
        let now = Instant::now();
        if next_frame < now {
            next_frame = now; // fell behind; don't try to catch up
        }

        let dt = now.duration_since(last).as_secs_f32().min(0.25);
        last = now;

        let (cpu, gpu) = (monitor.cpu(), monitor.gpu());
        let target = match args.fixed {
            Some(v) => v as f32 / 100.0,
            None => source.pick(cpu, gpu),
        };
        let k = if args.smooth > 0.0 { 1.0 - (-dt / args.smooth).exp() } else { 1.0 };
        smoothed += (target - smoothed) * k;

        if !paused {
            rain.step(dt, smoothed, intensity, args.speed.max(0.01));
        }

        // Compose the frame.
        let t = start.elapsed().as_secs_f32();
        let (base_hue, sat) = palette.hue_sat();
        let hue_for = |x: u16| -> f32 {
            if palette == Palette::Rainbow {
                base_hue + x as f32 * 4.0 + t * 30.0
            } else if heat {
                // Walk the short way round the color wheel towards red.
                let d = (0.0 - base_hue + 540.0).rem_euclid(360.0) - 180.0;
                base_hue + d * smoothed
            } else {
                base_hue
            }
        };
        let sat = if heat && palette == Palette::White { smoothed } else { sat };
        for x in 0..w {
            let hue = hue_for(x);
            for y in 0..h {
                let i = rain.idx(x, y);
                let c = rain.cells[i];
                back[i] = if c.life > 0.0 {
                    let (fg, bold) = painter.shade(hue, sat, c.life, c.head);
                    Glyph { ch: c.ch, fg, bg: Color::Reset, bold }
                } else {
                    BLANK
                };
            }
        }

        if hud && h > 0 {
            let gpu_txt = match gpu {
                Some(g) => format!("GPU {} {:3.0}%", bar(g, 10), g * 100.0),
                None => "GPU n/a".to_string(),
            };
            let src = match args.fixed {
                Some(_) => "fixed".to_string(),
                None => format!("{source:?}").to_lowercase(),
            };
            let text = format!(
                " CPU {} {:3.0}% │ {} │ rain[{}] {:3.0}% x{:.2}{} │ q h c t s +/- ",
                bar(cpu, 10),
                cpu * 100.0,
                gpu_txt,
                src,
                smoothed * 100.0,
                intensity,
                if paused { " paused" } else { "" },
            );
            let fg = painter.color(hsv(hue_for(0), sat * 0.3, 1.0));
            let bg = painter.color((0.0, 0.08, 0.0));
            for (x, ch) in text.chars().take(w as usize).enumerate() {
                back[x] = Glyph { ch, fg, bg, bold: true };
            }
        }

        screen.present(&back)?;
    }

    Ok(())
}
