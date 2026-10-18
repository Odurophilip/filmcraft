//! Video from a storyboard (`storyboard.build`): a scene plan becomes an editable sequence, in one
//! undo step. Each scene is a background (a colour, or a media file), an animated title and body
//! text, and a narration spoken with Text to Speech; captions follow the narration, scenes are
//! joined with a transition, and an optional music bed runs under them.
//!
//! The plan is plain JSON, so a person, the CLI or an agent (an LLM turning a brief into scenes)
//! can write it:
//!
//! ```json
//! {"name": "Launch", "format": "16:9", "voice": "basic-female",
//!  "scenes": [{"background": "#1d3557", "title": "Meet FilmCraft", "body": "Edit anywhere",
//!              "narration": "Meet FilmCraft, the editor that runs anywhere.", "animation": "fadeUp"}]}
//! ```
//!
//! Everything it makes is ordinary clips (colour mattes, graphic clips, narration clips, captions,
//! transitions), built with the same commands the UI uses, so the result is edited like any other
//! sequence. A scene lasts `seconds`, or as long as its narration plus a short pause. Without a
//! `voice`, narrations use a natural voice when those are downloaded, else the basic voice.
//!
//! A background may later be `{"generate": "<prompt>"}` (footage from a video model); this build
//! has no generator and says so.

use serde::Deserialize;
use serde_json::{Value, json};

use filmcraft_time::{TICKS_PER_SECOND, Tick};

use crate::commands::{CommandSpec, always, bad};
use crate::{EngineError, Result, Session};

const CMD: &str = "storyboard.build";
const MAX_SCENES: usize = 100;
/// Silence after a scene's narration before the next scene.
const NARRATION_PAD: f64 = 0.6;
const MIN_SCENE: f64 = 1.0;
const DEFAULT_SCENE: f64 = 4.0;
/// Fade of the scene text in and out.
const TEXT_FADE: f64 = 0.4;
/// The body text follows the title by this much.
const TEXT_STAGGER: f64 = 0.3;
/// Words per caption (tall frames: [`CAPTION_WORDS_TALL`]).
const CAPTION_WORDS: usize = 7;
const CAPTION_WORDS_TALL: usize = 4;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Plan {
    name: Option<String>,
    /// "16:9" (1920×1080), "9:16" (1080×1920), "1:1" (1080×1080), "4:5" (1080×1350).
    format: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    fps: Option<f64>,
    voice: Option<String>,
    pace: Option<f64>,
    /// Captions from the narration (default true).
    captions: Option<bool>,
    /// Video transition between scenes ("cross_dissolve" by default, "none" for cuts).
    transition: Option<String>,
    transition_seconds: Option<f64>,
    /// A music or ambience file under the whole video.
    music: Option<String>,
    /// Music level in dB (default −18).
    music_db: Option<f64>,
    style: Style,
    scenes: Vec<Scene>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Style {
    font: Option<String>,
    title_color: Option<String>,
    body_color: Option<String>,
    /// Default background colour for scenes that give none.
    background: Option<String>,
    /// Highlight ring colour (default: the title colour).
    accent: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Scene {
    background: Option<Background>,
    title: Option<String>,
    body: Option<String>,
    narration: Option<String>,
    seconds: Option<f64>,
    /// "fadeUp" (default), "fade", "pop", "slideLeft", "none".
    animation: Option<String>,
    voice: Option<String>,
    /// "center" (default) or "top": the text above a picture (a screenshot in the background).
    layout: Option<String>,
    /// Caption text when it should read differently from what the voice says (a name spelled
    /// for the voice, "dot io" for ".io"); default: the narration.
    caption: Option<String>,
    /// A picture over the background (a screenshot, a product shot) that animates in.
    image: Option<Image>,
    /// Rings that draw the eye to parts of the picture.
    highlights: Vec<Highlight>,
    /// The transition into this scene (default: the plan's).
    transition: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Image {
    /// An image or video file, as big as the frame (transparent PNGs keep their shadows).
    media: String,
    /// "slideUp" (default), "fade", "pop", "none".
    enter: Option<String>,
    /// Slow push-in after the entrance: the scale it ends at, in percent (default 103).
    zoom: Option<f64>,
    /// `media` is the first frame of a numbered image sequence (an animated picture).
    sequence: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Highlight {
    /// The ringed area in frame pixels (top-left, size), where it sits once the picture settles.
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    /// Seconds into the scene (default 1.5).
    at: Option<f64>,
    /// How long it stays (default: to the end of the scene, or until the next highlight).
    seconds: Option<f64>,
    color: Option<String>,
    /// Move in on the ringed area so it can be read on a phone (default true when the scene
    /// has a picture); the title steps aside while the camera is in close.
    zoom: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum Background {
    Color(String),
    Spec(BackgroundSpec),
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct BackgroundSpec {
    color: Option<String>,
    /// An image or video file.
    media: Option<String>,
    /// Keep the media's own sound (default false: the narration speaks alone).
    audio: bool,
    /// Footage to generate from a prompt (not available in this build).
    generate: Option<String>,
    /// Slow push-in on media: the scale it ends at, in percent (default 105; 100 = still).
    zoom: Option<f64>,
    /// `media` is the first frame of a numbered image sequence (an animation rendered to PNGs).
    sequence: bool,
}

fn is_hex_color(c: &str) -> bool {
    c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|ch| ch.is_ascii_hexdigit())
}

fn frame_size(p: &Plan) -> Result<(u32, u32)> {
    if let (Some(w), Some(h)) = (p.width, p.height) {
        if !(16..=8192).contains(&w) || !(16..=8192).contains(&h) {
            return Err(bad(CMD, "`width` and `height` must be 16 to 8192"));
        }
        return Ok((w, h));
    }
    Ok(match p.format.as_deref().unwrap_or("16:9") {
        "16:9" => (1920, 1080),
        "9:16" => (1080, 1920),
        "1:1" => (1080, 1080),
        "4:5" => (1080, 1350),
        f => return Err(bad(CMD, format!("unknown `format` `{f}` (16:9, 9:16, 1:1, 4:5, or give width and height)"))),
    })
}

/// Check the whole plan before anything is built, so a bad scene fails with nothing changed.
fn validate(p: &Plan) -> Result<()> {
    if p.scenes.is_empty() || p.scenes.len() > MAX_SCENES {
        return Err(bad(CMD, format!("`scenes` needs 1 to {MAX_SCENES} scenes")));
    }
    frame_size(p)?;
    if let Some(f) = p.fps
        && !(1.0..=120.0).contains(&f)
    {
        return Err(bad(CMD, "`fps` must be 1 to 120"));
    }
    for c in [&p.style.title_color, &p.style.body_color, &p.style.background, &p.style.accent].into_iter().flatten() {
        if !is_hex_color(c) {
            return Err(bad(CMD, format!("`{c}` is not a #rrggbb colour")));
        }
    }
    for (i, sc) in p.scenes.iter().enumerate() {
        let n = i + 1;
        if let Some(s) = sc.seconds
            && !(MIN_SCENE..=600.0).contains(&s)
        {
            return Err(bad(CMD, format!("scene {n}: `seconds` must be {MIN_SCENE} to 600")));
        }
        if let Some(a) = &sc.animation
            && !["fadeUp", "fade", "pop", "slideLeft", "none"].contains(&a.as_str())
        {
            return Err(bad(CMD, format!("scene {n}: unknown `animation` `{a}` (fadeUp, fade, pop, slideLeft, none)")));
        }
        if let Some(l) = &sc.layout
            && !["center", "top"].contains(&l.as_str())
        {
            return Err(bad(CMD, format!("scene {n}: unknown `layout` `{l}` (center, top)")));
        }
        if let Some(img) = &sc.image {
            if !std::path::Path::new(&img.media).is_file() {
                return Err(bad(CMD, format!("scene {n}: no such image file `{}`", img.media)));
            }
            if let Some(e) = &img.enter
                && !["slideUp", "fade", "pop", "none"].contains(&e.as_str())
            {
                return Err(bad(CMD, format!("scene {n}: unknown image `enter` `{e}` (slideUp, fade, pop, none)")));
            }
            if let Some(z) = img.zoom
                && !(100.0..=150.0).contains(&z)
            {
                return Err(bad(CMD, format!("scene {n}: image `zoom` must be 100 to 150")));
            }
        }
        for hl in &sc.highlights {
            if hl.w <= 0.0 || hl.h <= 0.0 {
                return Err(bad(CMD, format!("scene {n}: a highlight needs a positive `w` and `h`")));
            }
            if let Some(c) = &hl.color
                && !is_hex_color(c)
            {
                return Err(bad(CMD, format!("scene {n}: `{c}` is not a #rrggbb colour")));
            }
        }
        match &sc.background {
            Some(Background::Color(c)) if !is_hex_color(c) => return Err(bad(CMD, format!("scene {n}: `{c}` is not a #rrggbb colour"))),
            Some(Background::Spec(b)) => {
                if b.generate.is_some() {
                    return Err(bad(CMD, format!("scene {n}: generated footage is not available in this build; use a colour or a media file")));
                }
                if let Some(z) = b.zoom
                    && !(100.0..=150.0).contains(&z)
                {
                    return Err(bad(CMD, format!("scene {n}: `zoom` must be 100 to 150")));
                }
                if let Some(c) = &b.color
                    && !is_hex_color(c)
                {
                    return Err(bad(CMD, format!("scene {n}: `{c}` is not a #rrggbb colour")));
                }
                if let Some(m) = &b.media
                    && !std::path::Path::new(m).is_file()
                {
                    return Err(bad(CMD, format!("scene {n}: no such media file `{m}`")));
                }
            }
            _ => {}
        }
    }
    if let Some(m) = &p.music
        && !std::path::Path::new(m).is_file()
    {
        return Err(bad(CMD, format!("no such music file `{m}`")));
    }
    Ok(())
}

/// The narrator when the plan names none and the natural voices are downloaded.
const NATURAL_VOICE: &str = "kokoro-heart";

/// This build has the natural voices (Kokoro) and they are downloaded, so narrations need not use
/// the basic voice.
fn natural_voices() -> bool {
    cfg!(feature = "neural-voices") && crate::transcript::models_dir().is_some_and(|d| filmcraft_tts::catalog::installed(&d))
}

fn ticks(secs: f64) -> Tick {
    Tick((secs * TICKS_PER_SECOND as f64).round() as i64)
}

fn secs(t: Tick) -> f64 {
    t.0 as f64 / TICKS_PER_SECOND as f64
}

fn id_of(r: &Value, key: &str) -> Result<u64> {
    r[key].as_u64().ok_or_else(|| EngineError::Other(format!("{CMD}: expected `{key}` in {r}")))
}

/// Split narration into captions of about `per` words, timed by their share of the
/// words over the speech's length.
fn caption_chunks(text: &str, speech: f64, per: usize) -> Vec<(String, f64, f64)> {
    let words: Vec<&str> = text.split_whitespace().filter(|w| !w.starts_with("[pause")).collect();
    if words.is_empty() || speech <= 0.0 {
        return Vec::new();
    }
    let per_word = speech / words.len() as f64;
    words
        .chunks(per)
        .scan(0usize, |at, c| {
            let start = *at as f64 * per_word;
            *at += c.len();
            Some((c.join(" "), start, c.len() as f64 * per_word))
        })
        .collect()
}

/// Motion keys per eased move: enough that the straight segments between them read as a curve.
const EASE_KEYS: usize = 7;

fn lerp(a: &Value, b: &Value, k: f64) -> Value {
    match (a, b) {
        (Value::Array(x), Value::Array(y)) => Value::Array(x.iter().zip(y).map(|(p, q)| lerp(p, q, k)).collect()),
        _ => json!(a.as_f64().unwrap_or(0.0) + (b.as_f64().unwrap_or(0.0) - a.as_f64().unwrap_or(0.0)) * k),
    }
}

/// Keyframe `param` of `effect` on `clip` from `from` to `to` between `t0` and `t0 + len`, easing
/// out (fast, then settling), as a few keys a frame or more apart.
fn ease(s: &mut Session, clip: u64, effect: &str, param: &str, (from, to): (Value, Value), t0: Tick, len: Tick) -> Result<()> {
    let n = EASE_KEYS.min((len.0 / ticks(1.0 / 30.0).0.max(1)).max(1) as usize);
    for i in 0..=n {
        let x = i as f64 / n as f64;
        let k = 1.0 - (1.0 - x).powi(3);
        let at = t0 + Tick((len.0 as f64 * x).round() as i64);
        s.execute("effects.setParam", json!({"clip": clip, "effect": effect, "param": param, "value": lerp(&from, &to, k), "time": at.0, "keyframe": true}))?;
    }
    Ok(())
}

fn key(s: &mut Session, clip: u64, effect: &str, param: &str, t: Tick, value: Value) -> Result<()> {
    s.execute("effects.setParam", json!({"clip": clip, "effect": effect, "param": param, "value": value, "time": t.0, "keyframe": true})).map(|_| ())
}

/// Keyframe a text clip's entrance and exit (starting at `start`, `dur` long).
fn animate(s: &mut Session, clip: u64, start: Tick, dur: Tick, kind: &str, (w, h): (u32, u32)) -> Result<()> {
    if kind == "none" {
        return Ok(());
    }
    let fade = ticks(TEXT_FADE.min(secs(dur) / 4.0));
    let end = start + dur;
    key(s, clip, "opacity", "opacity", start, json!(0.0))?;
    key(s, clip, "opacity", "opacity", start + fade, json!(100.0))?;
    key(s, clip, "opacity", "opacity", end - fade, json!(100.0))?;
    key(s, clip, "opacity", "opacity", end, json!(0.0))?;
    let centre = json!([w as f64 / 2.0, h as f64 / 2.0]);
    let settle = ticks((TEXT_FADE * 2.0).min(secs(dur) / 3.0));
    match kind {
        "fadeUp" => ease(s, clip, "motion", "position", (json!([w as f64 / 2.0, h as f64 / 2.0 + h as f64 * 0.04]), centre), start, settle),
        "slideLeft" => ease(s, clip, "motion", "position", (json!([w as f64 / 2.0 + w as f64 * 0.08, h as f64 / 2.0]), centre), start, settle),
        "pop" => ease(s, clip, "motion", "scale", (json!(80.0), json!(100.0)), start, settle),
        _ => Ok(()),
    }
}

/// How long a picture takes to slide or pop in.
const IMAGE_ENTER: f64 = 0.9;

/// Animate a scene picture in (shown at `base` percent, so pictures bigger than the frame fit),
/// then push in slowly to `zoom` by the scene's end when `drift`.
#[allow(clippy::too_many_arguments)]
fn animate_image(s: &mut Session, clip: u64, start: Tick, dur: Tick, enter: &str, (base, zoom, drift): (f64, f64, bool), (w, h): (u32, u32)) -> Result<()> {
    let len = ticks(IMAGE_ENTER.min(secs(dur) / 3.0));
    let centre = json!([w as f64 / 2.0, h as f64 / 2.0]);
    if (base - 100.0).abs() > 1e-6 {
        key(s, clip, "motion", "scale", start, json!(base))?;
    }
    match enter {
        "slideUp" => {
            ease(s, clip, "motion", "position", (json!([w as f64 / 2.0, h as f64 * 0.95]), centre), start, len)?;
            key(s, clip, "opacity", "opacity", start, json!(0.0))?;
            key(s, clip, "opacity", "opacity", start + Tick(len.0 / 3), json!(100.0))?;
        }
        "pop" => {
            ease(s, clip, "motion", "scale", (json!(base * 0.88), json!(base)), start, len)?;
            key(s, clip, "opacity", "opacity", start, json!(0.0))?;
            key(s, clip, "opacity", "opacity", start + Tick(len.0 / 3), json!(100.0))?;
        }
        "fade" => {
            key(s, clip, "opacity", "opacity", start, json!(0.0))?;
            key(s, clip, "opacity", "opacity", start + len, json!(100.0))?;
        }
        _ => {}
    }
    if drift && zoom > 100.0 && enter != "pop" {
        key(s, clip, "motion", "scale", start + len, json!(base))?;
        key(s, clip, "motion", "scale", start + dur, json!(base * zoom / 100.0))?;
    }
    Ok(())
}

/// Camera moves for highlights: how long moving in and out take, the closest it gets, and the
/// room the ringed area may fill (frame pixels; the captions keep the bottom).
const ZOOM_IN: f64 = 0.7;
const ZOOM_OUT: f64 = 0.6;
const MAX_ZOOM: f64 = 2.8;
const ZOOM_ROOM: (f64, f64) = (0.78, 0.6);

/// The picture's scale at `t` into a scene (its push-in after the entrance).
fn zoom_at(t: f64, dur: f64, zoom: f64) -> f64 {
    let a = IMAGE_ENTER.min(dur / 3.0);
    if t <= a || dur <= a { 1.0 } else { 1.0 + (zoom / 100.0 - 1.0) * ((t - a) / (dur - a)).min(1.0) }
}

fn build(s: &mut Session, p: &Value) -> Result<Value> {
    let plan_v = p.get("plan").unwrap_or(p);
    let plan: Plan = serde_json::from_value(plan_v.clone()).map_err(|e| bad(CMD, format!("bad plan: {e}")))?;
    validate(&plan)?;
    let (w, h) = frame_size(&plan)?;
    let name = plan.name.clone().unwrap_or_else(|| "Storyboard".into());
    let fps = plan.fps.unwrap_or(30.0);
    s.edit_group("Build Video from Storyboard", |s| {
        let r = s.execute("file.newSequence", json!({"name": name, "width": w, "height": h, "fps": fps, "video": 5, "audio": 2}))?;
        let sequence = id_of(&r, "sequence")?;
        let rate = s.active_sequence().ok_or(EngineError::NoSequence)?.settings.frame_rate;
        let snap = |t: Tick| {
            let fd = rate.frame_duration().0.max(1);
            Tick((t.0 + fd - 1) / fd * fd)
        };
        let caption_track = if plan.captions.unwrap_or(true) && plan.scenes.iter().any(|sc| sc.narration.is_some()) {
            let track = id_of(&s.execute("captions.newTrack", json!({"format": "Subtitle", "name": "Narration"}))?, "track")?;
            if h > w {
                // caption size follows the frame height: keep it in proportion to a tall frame's width
                let size = (54.0 * w as f64 / h as f64 * 1.2).round();
                s.execute("captions.setStyle", json!({"track": track, "size": size}))?;
            }
            Some(track)
        } else {
            None
        };
        let title_size = (w.min(h) as f64 * 0.1).round();
        let body_size = (w.min(h) as f64 * 0.045).round();
        let (box_w, box_x) = (w as f64 * 0.84, w as f64 * 0.08);
        let mut t = Tick::ZERO;
        let mut report = Vec::new();
        let mut backgrounds: Vec<(Option<u64>, Option<&str>)> = Vec::new();
        for sc in &plan.scenes {
            // narration first: it sets the scene's length
            let mut narration = None;
            let mut speech = 0.0;
            if let Some(text) = sc.narration.as_deref().filter(|x| !x.trim().is_empty()) {
                let mut q = json!({"text": text, "time": t.0, "track": "A1"});
                match sc.voice.as_ref().or(plan.voice.as_ref()) {
                    Some(v) => q["voice"] = json!(v),
                    None if natural_voices() => q["voice"] = json!(NATURAL_VOICE),
                    None => {}
                }
                if let Some(pace) = plan.pace {
                    q["pace"] = json!(pace);
                }
                let r = s.execute("tts.create", q)?;
                speech = secs(Tick(r["speechDuration"].as_i64().unwrap_or(0)));
                narration = Some(id_of(&r, "clip")?);
            }
            let want = sc.seconds.unwrap_or(if narration.is_some() { 0.0 } else { DEFAULT_SCENE });
            let dur = snap(ticks(want.max(speech + if narration.is_some() { NARRATION_PAD } else { 0.0 }).max(MIN_SCENE)));
            let seconds = secs(dur);

            // background on V1
            let (color, media, keep_audio, zoom, sequence) = match &sc.background {
                Some(Background::Color(c)) => (Some(c.clone()), None, false, 100.0, false),
                Some(Background::Spec(b)) => (b.color.clone(), b.media.clone(), b.audio, b.zoom.unwrap_or(if b.sequence { 100.0 } else { 105.0 }), b.sequence),
                None => (None, None, false, 100.0, false),
            };
            let bg = match media {
                Some(path) => {
                    let r = s.execute("file.import", json!({"paths": [path], "imageSequence": sequence}))?;
                    let item = r["items"][0].as_u64().ok_or_else(|| EngineError::Other(format!("{CMD}: could not import `{path}`")))?;
                    let r = s.execute(
                        "timeline.place",
                        json!({"item": item, "track": "V1", "time": t.0, "duration": dur.0, "audio": keep_audio, "audioTrack": "A2"}),
                    )?;
                    let clip = r["clips"][0].as_u64().or_else(|| r["clip"].as_u64());
                    if let Some(c) = clip
                        && zoom > 100.0
                    {
                        // a slow push-in keeps a still picture alive
                        for (at, scale) in [(t, 100.0), (t + dur, zoom)] {
                            s.execute(
                                "effects.setParam",
                                json!({"clip": c, "effect": "motion", "param": "scale", "value": scale, "time": at.0, "keyframe": true}),
                            )?;
                        }
                    }
                    clip
                }
                None => {
                    let color = color.or_else(|| plan.style.background.clone()).unwrap_or_else(|| "#101820".into());
                    let r = s.execute("file.newColorMatte", json!({"color": color, "seconds": seconds}))?;
                    let item = id_of(&r, "item")?;
                    let r = s.execute("timeline.place", json!({"item": item, "track": "V1", "time": t.0, "duration": dur.0}))?;
                    r["clips"][0].as_u64().or_else(|| r["clip"].as_u64())
                }
            };
            backgrounds.push((bg, sc.transition.as_deref()));

            // the picture on V2: slides in over the background, then drifts
            let mut image = None;
            let image_zoom = sc.image.as_ref().map_or(100.0, |i| i.zoom.unwrap_or(103.0));
            let camera = sc.image.is_some() && sc.highlights.iter().any(|hl| hl.zoom.unwrap_or(true));
            let mut base = 100.0;
            if let Some(img) = &sc.image {
                let r = s.execute("file.import", json!({"paths": [img.media], "imageSequence": img.sequence}))?;
                let item = r["items"][0].as_u64().ok_or_else(|| EngineError::Other(format!("{CMD}: could not import `{}`", img.media)))?;
                // a picture bigger than the frame (a 2× render, sharp when the camera moves in) fits it
                if let Some((iw, ih)) = s.source(filmcraft_project::ItemId(item)).and_then(|src| src.info().video.as_ref().map(|v| (v.width, v.height))) {
                    base = 100.0 * (w as f64 / iw.max(1) as f64).min(h as f64 / ih.max(1) as f64).min(1.0);
                }
                let r = s.execute("timeline.place", json!({"item": item, "track": "V2", "time": t.0, "duration": dur.0, "audio": false}))?;
                if let Some(c) = r["clips"][0].as_u64().or_else(|| r["clip"].as_u64()) {
                    animate_image(s, c, t, dur, img.enter.as_deref().unwrap_or("slideUp"), (base, image_zoom, !camera), (w, h))?;
                    image = Some(c);
                }
            }

            // title and body: one graphic clip on V2, text wrapped in a centred box
            let mut text_clip = None;
            let mut text_clips = Vec::new();
            let title = sc.title.as_deref().filter(|x| !x.trim().is_empty());
            let body = sc.body.as_deref().filter(|x| !x.trim().is_empty());
            let top_layout = sc.layout.as_deref() == Some("top");
            let (title_top, body_top) = match (top_layout, body.is_some()) {
                (true, _) => (0.045, 0.165),
                (false, true) => (0.30, 0.54),
                (false, false) => (0.36, 0.54),
            };
            let scale = if top_layout { 0.8 } else { 1.0 };
            for (k, (text, size, color, top)) in [
                (title, (title_size * scale).round(), plan.style.title_color.as_deref().unwrap_or("#ffffff"), title_top),
                (body, body_size, plan.style.body_color.as_deref().unwrap_or("#d9e2ec"), body_top),
            ]
            .into_iter()
            .enumerate()
            {
                let Some(text) = text else { continue };
                let box_h = match (k, top_layout) {
                    (0, true) => h as f64 * 0.12,
                    (0, false) => h as f64 * 0.30,
                    (_, true) => h as f64 * 0.08,
                    _ => h as f64 * 0.2,
                };
                let mut q = json!({"text": text, "size": size, "box": [box_w, box_h], "position": [box_x, h as f64 * top]});
                if let Some(f) = &plan.style.font {
                    q["font"] = json!(f);
                }
                if k == 0 {
                    q["fontStyle"] = json!("Bold");
                }
                // the title on V3, the body on V4 a beat later
                let delay = if k == 0 { Tick::ZERO } else { ticks(TEXT_STAGGER.min(seconds / 6.0)) };
                q["seconds"] = json!(seconds);
                q["time"] = json!(t.0);
                q["track"] = json!(2 + k);
                let r = s.execute("graphics.newText", q)?;
                let clip = id_of(&r, "clip")?;
                let layer = r["layer"].as_u64().unwrap_or(0);
                s.execute("graphics.set", json!({"clip": clip, "layer": layer, "props": {"fill_color": color, "align": "center"}}))?;
                animate(s, clip, t + delay, dur - delay, sc.animation.as_deref().unwrap_or("fadeUp"), (w, h))?;
                text_clip.get_or_insert(clip);
                text_clips.push(clip);
            }

            // highlight rings on V5. With a picture the camera moves in on each one, so it reads on a
            // phone, and the title steps aside meanwhile; otherwise the ring sits where the
            // picture is at that moment.
            let accent = plan.style.accent.as_deref().or(plan.style.title_color.as_deref()).unwrap_or("#ffffff");
            let mut order: Vec<&Highlight> = sc.highlights.iter().collect();
            order.sort_by(|a, b| a.at.unwrap_or(1.5).total_cmp(&b.at.unwrap_or(1.5)));
            let (cx, cy) = (w as f64 / 2.0, h as f64 / 2.0);
            let pad = 10.0;
            let entered = IMAGE_ENTER.min(seconds / 3.0) + 0.1;
            for (i, hl) in order.iter().enumerate() {
                let next = order.get(i + 1).map_or(seconds, |n| n.at.unwrap_or(1.5));
                let mut at = hl.at.unwrap_or(1.5).clamp(0.0, (seconds - 0.5).max(0.0));
                let close_up = match image {
                    Some(img) if hl.zoom.unwrap_or(true) => {
                        at = at.max(entered);
                        let latest = (next - ZOOM_OUT).min(seconds - TEXT_FADE - ZOOM_OUT - 0.1);
                        let end = hl.seconds.map_or(latest, |d| (at + d).min(latest));
                        (end >= at + ZOOM_IN + 0.6).then_some((img, end))
                    }
                    _ => None,
                };
                let (rw, rh) = (hl.w + pad * 2.0, hl.h + pad * 2.0);
                let region = [hl.x + hl.w / 2.0, hl.y + hl.h / 2.0];
                let (ring_at, ring_len, centre, size) = match close_up {
                    Some((img, end)) => {
                        let z = (w as f64 * ZOOM_ROOM.0 / rw).min(h as f64 * ZOOM_ROOM.1 / rh).clamp(1.0, MAX_ZOOM);
                        let target = [cx, h as f64 * 0.47];
                        let moved = json!([target[0] - z * (region[0] - cx), target[1] - z * (region[1] - cy)]);
                        let (t_in, t_end) = (t + ticks(at), t + ticks(end));
                        key(s, img, "motion", "scale", t_in, json!(base))?;
                        key(s, img, "motion", "position", t_in, json!([cx, cy]))?;
                        ease(s, img, "motion", "scale", (json!(base), json!(base * z)), t_in, ticks(ZOOM_IN))?;
                        ease(s, img, "motion", "position", (json!([cx, cy]), moved.clone()), t_in, ticks(ZOOM_IN))?;
                        key(s, img, "motion", "scale", t_end, json!(base * z))?;
                        key(s, img, "motion", "position", t_end, moved.clone())?;
                        ease(s, img, "motion", "scale", (json!(base * z), json!(base)), t_end, ticks(ZOOM_OUT))?;
                        ease(s, img, "motion", "position", (moved, json!([cx, cy])), t_end, ticks(ZOOM_OUT))?;
                        for &c in &text_clips {
                            key(s, c, "opacity", "opacity", t_in, json!(100.0))?;
                            key(s, c, "opacity", "opacity", t_in + ticks(0.35), json!(0.0))?;
                            key(s, c, "opacity", "opacity", t_end, json!(0.0))?;
                            key(s, c, "opacity", "opacity", t_end + ticks(ZOOM_OUT), json!(100.0))?;
                        }
                        (at + ZOOM_IN, end - at - ZOOM_IN, target, [rw * z, rh * z])
                    }
                    None => {
                        let len = hl.seconds.unwrap_or(seconds - at).clamp(0.5, (seconds - at).max(0.5));
                        let z = if image.is_some() && !camera { zoom_at(at, seconds, image_zoom) } else { 1.0 };
                        (at, len, [cx + (region[0] - cx) * z, cy + (region[1] - cy) * z], [rw * z, rh * z])
                    }
                };
                let start = snap(t + ticks(ring_at));
                let r = s.execute("graphics.newRectangle", json!({"position": centre, "size": size, "seconds": ring_len, "time": start.0, "track": 4}))?;
                let ring = id_of(&r, "clip")?;
                let color = hl.color.as_deref().unwrap_or(accent);
                s.execute(
                    "graphics.set",
                    json!({"clip": ring, "layer": 0, "props": {"fill": false, "stroke": true, "stroke_color": color, "stroke_width": 7, "corner_radius": 18}}),
                )?;
                let pop = ticks(0.35);
                ease(s, ring, "motion", "scale", (json!(118.0), json!(100.0)), start, pop)?;
                let end = start + ticks(ring_len);
                key(s, ring, "opacity", "opacity", start, json!(0.0))?;
                key(s, ring, "opacity", "opacity", start + pop, json!(100.0))?;
                key(s, ring, "opacity", "opacity", end - ticks(0.2), json!(100.0))?;
                key(s, ring, "opacity", "opacity", end, json!(0.0))?;
            }

            // captions from the narration
            let mut captions = 0;
            if let (Some(track), Some(text)) = (caption_track, sc.caption.as_deref().or(sc.narration.as_deref())) {
                // boundaries on whole frames, so neighbouring captions never overlap
                let fd = rate.frame_duration().0.max(1);
                let frame = |x: f64| Tick((ticks(x).0 as f64 / fd as f64).round() as i64 * fd);
                for (line, at, len) in caption_chunks(text, speech, if h > w { CAPTION_WORDS_TALL } else { CAPTION_WORDS }) {
                    let (a, b) = (t + frame(at), t + frame(at + len));
                    if b <= a {
                        continue;
                    }
                    s.execute("captions.add", json!({"track": track, "text": line, "time": a.0, "durationSeconds": secs(b - a)}))?;
                    captions += 1;
                }
            }
            report.push(
                json!({"start": t.0, "duration": dur.0, "seconds": seconds, "background": bg, "image": image, "text": text_clip, "narration": narration, "captions": captions}),
            );
            t += dur;
        }

        // transitions between scene backgrounds
        let mut warnings = Vec::new();
        let frames = ((plan.transition_seconds.unwrap_or(0.5).clamp(0.0, 5.0)) * fps).round().max(1.0) as i64;
        for (c, own) in backgrounds.iter().skip(1) {
            let transition = own.or(plan.transition.as_deref()).unwrap_or("cross_dissolve");
            if transition == "none" {
                continue;
            }
            if let Some(c) = c
                && let Err(e) = s.execute("sequence.applyVideoTransition", json!({"clip": c, "effect": transition, "frames": frames, "edge": "in"}))
            {
                warnings.push(format!("transition: {e}"));
            }
        }

        // music bed on A2
        if let Some(path) = &plan.music {
            let r = s.execute("file.import", json!({"paths": [path]}))?;
            if let Some(item) = r["items"][0].as_u64() {
                let r = s.execute("timeline.place", json!({"item": item, "track": "A2", "audioTrack": "A2", "video": false, "time": 0, "duration": t.0}))?;
                if let Some(c) = r["clips"][0].as_u64().or_else(|| r["clip"].as_u64()) {
                    s.execute("effects.setParam", json!({"clip": c, "effect": "volume", "param": "level", "value": plan.music_db.unwrap_or(-18.0)}))?;
                }
            }
        }
        s.execute("playhead.set", json!({"time": 0})).ok();
        Ok(json!({"sequence": sequence, "duration": t.0, "seconds": secs(t), "width": w, "height": h, "scenes": report, "warnings": warnings}))
    })
}

pub fn commands() -> Vec<CommandSpec> {
    vec![CommandSpec {
        id: CMD,
        label: "New Video from Storyboard…",
        menu: &[],
        shortcut: None,
        params: r##"{"plan":{"name":str?,"format":"16:9|9:16|1:1|4:5"?,"width":u32?,"height":u32?,"fps":f64=30,"voice":str?,"pace":0.5..2?,"captions":bool=true,"transition":"cross_dissolve|none|<video transition>"?,"transition_seconds":f64=0.5,"music":path?,"music_db":f64=-18,"style":{"font":str?,"title_color":"#rrggbb"?,"body_color":"#rrggbb"?,"background":"#rrggbb"?,"accent":"#rrggbb"?}?,"scenes":[{"background":"#rrggbb"|{"color":"#rrggbb"?,"media":path?,"audio":bool?,"zoom":100..150?,"sequence":bool?}?,"title":str?,"body":str?,"narration":str?,"seconds":f64?,"animation":"fadeUp|fade|pop|slideLeft|none"?,"voice":str?,"layout":"center|top"?,"caption":str?,"image":{"media":path,"enter":"slideUp|fade|pop|none"?,"zoom":100..150?,"sequence":bool?}?,"highlights":[{"x":px,"y":px,"w":px,"h":px,"at":s?,"seconds":s?,"color":"#rrggbb"?,"zoom":bool=true}]?,"transition":str?}]}} (or the plan itself as the params)"##,
        enabled: always,
        run: build,
        journal: true,
    }]
}
