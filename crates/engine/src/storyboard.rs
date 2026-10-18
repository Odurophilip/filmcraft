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
    for c in [&p.style.title_color, &p.style.body_color, &p.style.background].into_iter().flatten() {
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

/// Keyframe the scene text's entrance and exit on graphic clip `clip` (starting at `start`,
/// `dur` long).
fn animate(s: &mut Session, clip: u64, start: Tick, dur: Tick, kind: &str, (w, h): (u32, u32)) -> Result<()> {
    if kind == "none" {
        return Ok(());
    }
    let fade = ticks(TEXT_FADE.min(secs(dur) / 4.0));
    let end = start + dur;
    let key = |s: &mut Session, effect: &str, param: &str, t: Tick, value: Value| {
        s.execute("effects.setParam", json!({"clip": clip, "effect": effect, "param": param, "value": value, "time": t.0, "keyframe": true})).map(|_| ())
    };
    key(s, "opacity", "opacity", start, json!(0.0))?;
    key(s, "opacity", "opacity", start + fade, json!(100.0))?;
    key(s, "opacity", "opacity", end - fade, json!(100.0))?;
    key(s, "opacity", "opacity", end, json!(0.0))?;
    let centre = [w as f64 / 2.0, h as f64 / 2.0];
    let settle = start + ticks((TEXT_FADE * 2.0).min(secs(dur) / 3.0));
    match kind {
        "fadeUp" => {
            key(s, "motion", "position", start, json!([centre[0], centre[1] + h as f64 * 0.04]))?;
            key(s, "motion", "position", settle, json!(centre))?;
        }
        "slideLeft" => {
            key(s, "motion", "position", start, json!([centre[0] + w as f64 * 0.08, centre[1]]))?;
            key(s, "motion", "position", settle, json!(centre))?;
        }
        "pop" => {
            key(s, "motion", "scale", start, json!(80.0))?;
            key(s, "motion", "scale", settle, json!(100.0))?;
        }
        _ => {}
    }
    Ok(())
}

fn build(s: &mut Session, p: &Value) -> Result<Value> {
    let plan_v = p.get("plan").unwrap_or(p);
    let plan: Plan = serde_json::from_value(plan_v.clone()).map_err(|e| bad(CMD, format!("bad plan: {e}")))?;
    validate(&plan)?;
    let (w, h) = frame_size(&plan)?;
    let name = plan.name.clone().unwrap_or_else(|| "Storyboard".into());
    let fps = plan.fps.unwrap_or(30.0);
    s.edit_group("Build Video from Storyboard", |s| {
        let r = s.execute("file.newSequence", json!({"name": name, "width": w, "height": h, "fps": fps, "video": 3, "audio": 2}))?;
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
        let mut backgrounds = Vec::new();
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
            let (color, media, keep_audio, zoom) = match &sc.background {
                Some(Background::Color(c)) => (Some(c.clone()), None, false, 100.0),
                Some(Background::Spec(b)) => (b.color.clone(), b.media.clone(), b.audio, b.zoom.unwrap_or(105.0)),
                None => (None, None, false, 100.0),
            };
            let bg = match media {
                Some(path) => {
                    let r = s.execute("file.import", json!({"paths": [path]}))?;
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
            backgrounds.extend(bg);

            // title and body: one graphic clip on V2, text wrapped in a centred box
            let mut text_clip = None;
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
                match text_clip {
                    Some(c) => q["clip"] = json!(c),
                    None => {
                        q["seconds"] = json!(seconds);
                        q["time"] = json!(t.0);
                        q["track"] = json!(1);
                    }
                }
                let r = s.execute("graphics.newText", q)?;
                let clip = id_of(&r, "clip")?;
                let layer = r["layer"].as_u64().unwrap_or(0);
                s.execute("graphics.set", json!({"clip": clip, "layer": layer, "props": {"fill_color": color, "align": "center"}}))?;
                text_clip = Some(clip);
            }
            if let Some(c) = text_clip {
                animate(s, c, t, dur, sc.animation.as_deref().unwrap_or("fadeUp"), (w, h))?;
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
                json!({"start": t.0, "duration": dur.0, "seconds": seconds, "background": bg, "text": text_clip, "narration": narration, "captions": captions}),
            );
            t += dur;
        }

        // transitions between scene backgrounds
        let transition = plan.transition.as_deref().unwrap_or("cross_dissolve");
        let mut warnings = Vec::new();
        if transition != "none" {
            let frames = ((plan.transition_seconds.unwrap_or(0.5).clamp(0.0, 5.0)) * fps).round().max(1.0) as i64;
            for c in backgrounds.iter().skip(1) {
                if let Err(e) = s.execute("sequence.applyVideoTransition", json!({"clip": c, "effect": transition, "frames": frames, "edge": "in"})) {
                    warnings.push(format!("transition: {e}"));
                }
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
        params: r##"{"plan":{"name":str?,"format":"16:9|9:16|1:1|4:5"?,"width":u32?,"height":u32?,"fps":f64=30,"voice":str?,"pace":0.5..2?,"captions":bool=true,"transition":"cross_dissolve|none|<video transition>"?,"transition_seconds":f64=0.5,"music":path?,"music_db":f64=-18,"style":{"font":str?,"title_color":"#rrggbb"?,"body_color":"#rrggbb"?,"background":"#rrggbb"?}?,"scenes":[{"background":"#rrggbb"|{"color":"#rrggbb"?,"media":path?,"audio":bool?,"zoom":100..150?}?,"title":str?,"body":str?,"narration":str?,"seconds":f64?,"animation":"fadeUp|fade|pop|slideLeft|none"?,"voice":str?,"layout":"center|top"?,"caption":str?}]}} (or the plan itself as the params)"##,
        enabled: always,
        run: build,
        journal: true,
    }]
}
