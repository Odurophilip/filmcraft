//! Video from a prompt (`storyboard.fromPrompt`): Claude writes a storyboard plan from a brief, and
//! [`crate::storyboard`] builds it into an editable sequence.
//!
//! The brief goes to the Claude API (`claude-opus-5-5`) with a strict JSON schema, so the reply is
//! always a plan `storyboard.build` can read. The request runs as a background job with a status
//! line and Cancel; when it finishes, [`poll`] builds the video (one undo step). Options the user
//! chose (format, voice, captions, music) are merged over Claude's plan.
//!
//! The API key comes from `ANTHROPIC_API_KEY`, else from `<data dir>/anthropic-api-key` (written
//! by `storyboard.setApiKey`, readable by the user only). Needs the engine feature `storyboard-ai`
//! for HTTPS; hosts and tests can install a [`Planner`] in [`Session::storyboard_planner`]
//! instead, which needs neither the feature nor a key.

use std::sync::{Arc, Mutex, PoisonError};

use serde_json::{Value, json};

use crate::commands::{CommandSpec, always, bad, bool_p, str_p};
use crate::{EngineError, Result, Session};

const CMD: &str = "storyboard.fromPrompt";
const MODEL: &str = "claude-opus-5-5";
#[cfg(feature = "storyboard-ai")]
const API_URL: &str = "https://api.anthropic.com/v1/messages";
const KEY_FILE: &str = "anthropic-api-key";
const MAX_BRIEF: usize = 4000;
const MAX_SCENES: usize = 20;

/// Turns a Messages API request body into the API's JSON response (the network call, or a fake).
pub type Planner = Arc<dyn Fn(&Value) -> std::result::Result<Value, String> + Send + Sync>;

/// A `storyboard.fromPrompt` job whose plan is built when it finishes.
pub struct PendingPrompt {
    pub job: u64,
    /// The user's options, merged over Claude's plan.
    pub options: Value,
    pub plan: Arc<Mutex<Option<Value>>>,
}

fn key_path() -> Option<std::path::PathBuf> {
    crate::autosave::default_data_dir().map(|d| d.join(KEY_FILE))
}

/// The Anthropic API key: `ANTHROPIC_API_KEY`, else the saved key file.
pub fn api_key() -> Option<String> {
    if let Ok(k) = std::env::var("ANTHROPIC_API_KEY")
        && !k.trim().is_empty()
    {
        return Some(k.trim().to_string());
    }
    let k = std::fs::read_to_string(key_path()?).ok()?;
    Some(k.trim().to_string()).filter(|k| !k.is_empty())
}

/// `storyboard.fromPrompt` can run: a planner is installed, or this build can call Claude and has
/// a key.
pub fn ready(s: &Session) -> std::result::Result<(), String> {
    if s.storyboard_planner.is_some() {
        return Ok(());
    }
    if !cfg!(feature = "storyboard-ai") {
        return Err("video from a prompt is not available in this build (built without the `storyboard-ai` feature)".into());
    }
    if api_key().is_none() {
        return Err("no Anthropic API key: set ANTHROPIC_API_KEY or save one with storyboard.setApiKey".into());
    }
    Ok(())
}

fn set_api_key(_: &mut Session, p: &Value) -> Result<Value> {
    let cmd = "storyboard.setApiKey";
    let key = str_p(p, "key").ok_or_else(|| bad(cmd, "`key` is required (an empty key removes the saved one)"))?.trim();
    let path = key_path().ok_or_else(|| bad(cmd, "no data directory to save the key in"))?;
    if key.is_empty() {
        let _ = std::fs::remove_file(&path);
        return Ok(json!({"saved": false}));
    }
    if !key.starts_with("sk-ant-") {
        return Err(bad(cmd, "that does not look like an Anthropic API key (sk-ant-…)"));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| EngineError::Other(e.to_string()))?;
    }
    std::fs::write(&path, key).map_err(|e| EngineError::Other(e.to_string()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(json!({"saved": true}))
}

/// The plan Claude must return (strict structured output).
fn schema() -> Value {
    let s = |d: &str| json!({"type": "string", "description": d});
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["name", "style", "scenes"],
        "properties": {
            "name": s("A short name for the video"),
            "style": {
                "type": "object",
                "additionalProperties": false,
                "required": ["title_color", "body_color"],
                "properties": {"title_color": s("#rrggbb"), "body_color": s("#rrggbb")}
            },
            "scenes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["background", "title", "body", "narration", "animation"],
                    "properties": {
                        "background": s("Background colour, #rrggbb"),
                        "title": s("On-screen headline"),
                        "body": s("On-screen supporting line, or empty"),
                        "narration": s("What the narrator says during the scene"),
                        "animation": {"type": "string", "enum": ["fadeUp", "fade", "pop", "slideLeft", "none"]}
                    }
                }
            }
        }
    })
}

const SYSTEM: &str = "You are a video storyboard writer for FilmCraft, a video editor. From the user's brief, write a \
short narrated video as a sequence of scenes. Each scene is a solid background colour with an on-screen headline, \
an optional supporting line, and narration read aloud by a text-to-speech voice.

Write for the requested length: narration is spoken at about 2.5 words per second, and each scene lasts as long as \
its narration. Use 3 to 8 scenes. Keep headlines to 6 words or fewer and supporting lines to 10 words or fewer; \
the screen should reinforce the narration, not repeat it word for word. Narration is plain spoken sentences: no \
markdown, emoji, hashtags, URLs or symbols a voice would read awkwardly. Open with a hook and end with a clear \
closing line or call to action.

Choose a cohesive palette that fits the subject. Every background must keep the title and body colours easy to \
read (strong contrast). Vary the animations across scenes.";

/// The Messages API request for a brief.
pub fn request_body(brief: &str, seconds: Option<f64>, format: &str) -> Value {
    let length = seconds.map_or("about 30 seconds".to_string(), |s| format!("about {s:.0} seconds"));
    let frame = match format {
        "9:16" => "vertical 9:16 (phone, short lines work best)",
        "1:1" => "square 1:1",
        "4:5" => "portrait 4:5",
        _ => "widescreen 16:9",
    };
    json!({
        "model": MODEL,
        "max_tokens": 16000,
        "system": SYSTEM,
        "output_config": {"effort": "medium", "format": {"type": "json_schema", "schema": schema()}},
        "fallbacks": "default",
        "messages": [{"role": "user", "content": format!("Brief: {brief}\n\nLength: {length}. Frame: {frame}.")}]
    })
}

fn is_hex(c: &str) -> bool {
    c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|ch| ch.is_ascii_hexdigit())
}

/// The storyboard plan in a Messages API response, checked and cleaned for `storyboard.build`.
pub fn plan_from_response(resp: &Value) -> std::result::Result<Value, String> {
    if let Some(e) = resp.get("error") {
        return Err(format!("Claude API: {}", e["message"].as_str().unwrap_or("request failed")));
    }
    match resp["stop_reason"].as_str() {
        Some("refusal") => {
            let why = resp["stop_details"]["explanation"].as_str().unwrap_or("try rewording the brief");
            return Err(format!("Claude declined this brief ({why})"));
        }
        Some("max_tokens") => return Err("the storyboard was cut off; ask for a shorter video".into()),
        _ => {}
    }
    let text: String = resp["content"].as_array().into_iter().flatten().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect();
    let mut plan: Value = serde_json::from_str(text.trim()).map_err(|e| format!("Claude's storyboard was not valid JSON: {e}"))?;
    let scenes = plan["scenes"].as_array_mut().ok_or("Claude's storyboard has no scenes")?;
    scenes.truncate(MAX_SCENES);
    scenes.retain(|sc| ["title", "body", "narration"].iter().any(|k| sc[k].as_str().is_some_and(|t| !t.trim().is_empty())));
    if scenes.is_empty() {
        return Err("Claude's storyboard has no scenes".into());
    }
    for sc in scenes.iter_mut() {
        if !sc["background"].as_str().is_some_and(is_hex)
            && let Some(o) = sc.as_object_mut()
        {
            o.remove("background");
        }
    }
    if let Some(style) = plan["style"].as_object_mut() {
        style.retain(|_, v| v.as_str().is_some_and(is_hex));
    }
    Ok(plan)
}

/// The real planner: POST to the Messages API.
#[cfg(feature = "storyboard-ai")]
fn call_claude(body: &Value) -> std::result::Result<Value, String> {
    let key = api_key().ok_or("no Anthropic API key")?;
    let headers = [("x-api-key", key.as_str()), ("anthropic-version", "2023-06-01"), ("anthropic-beta", "server-side-fallback-2026-07-01")];
    let (status, text) =
        filmcraft_speech::models::post_json(API_URL, &headers, &body.to_string(), std::time::Duration::from_secs(300)).map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&text).map_err(|_| format!("Claude API returned HTTP {status}"))?;
    if status >= 400 && v.get("error").is_none() {
        return Err(format!("Claude API returned HTTP {status}"));
    }
    Ok(v)
}

#[cfg(not(feature = "storyboard-ai"))]
fn call_claude(_: &Value) -> std::result::Result<Value, String> {
    Err("video from a prompt is not available in this build".into())
}

/// The user's options that override Claude's plan.
fn options(p: &Value) -> Value {
    let mut o = json!({});
    for k in ["format", "voice", "pace", "captions", "transition", "music", "music_db", "fps"] {
        if let Some(v) = p.get(k) {
            o[k] = v.clone();
        }
    }
    o
}

fn from_prompt(s: &mut Session, p: &Value) -> Result<Value> {
    use std::sync::atomic::Ordering;
    let brief = str_p(p, "prompt").map(str::trim).filter(|b| !b.is_empty()).ok_or_else(|| bad(CMD, "`prompt` is required"))?;
    if brief.len() > MAX_BRIEF {
        return Err(bad(CMD, format!("`prompt` is limited to {MAX_BRIEF} characters")));
    }
    ready(s).map_err(EngineError::Other)?;
    if !s.storyboard_jobs.is_empty() {
        return Err(EngineError::Other("a video is already being written".into()));
    }
    let seconds = p.get("seconds").and_then(Value::as_f64).map(|x| x.clamp(5.0, 600.0));
    let body = request_body(brief, seconds, str_p(p, "format").unwrap_or("16:9"));
    let planner: Planner = s.storyboard_planner.clone().unwrap_or_else(|| Arc::new(call_claude));
    let job = crate::narration::new_job(s, "Writing the storyboard with Claude".into());
    let plan: Arc<Mutex<Option<Value>>> = Arc::default();
    let (prog, res, out) = (job.progress.clone(), job.result.clone(), plan.clone());
    let wait = bool_p(p, "wait").unwrap_or(false);
    let id = crate::narration::spawn(s, job, wait, "filmcraft-storyboard-ai", move || {
        let t0 = web_time::Instant::now();
        *prog.status.lock().unwrap_or_else(PoisonError::into_inner) = "Writing the storyboard with Claude…".into();
        let r = planner(&body).and_then(|resp| plan_from_response(&resp)).map(|pl| {
            let n = pl["scenes"].as_array().map_or(0, Vec::len);
            *out.lock().unwrap_or_else(PoisonError::into_inner) = Some(pl);
            format!("Storyboard ready: {n} scenes")
        });
        prog.done.store(1, Ordering::Relaxed);
        prog.total.store(1, Ordering::Relaxed);
        crate::narration::finish(&prog, &res, r, t0);
    })?;
    s.storyboard_jobs.push(PendingPrompt { job: id, options: options(p), plan });
    if wait {
        let pending = s.storyboard_jobs.pop().ok_or_else(|| EngineError::Other("the job disappeared".into()))?;
        if let Some(Err(e)) = s.jobs.iter().find(|j| j.id == id).and_then(|j| j.result.lock().unwrap_or_else(PoisonError::into_inner).clone()) {
            return Err(EngineError::Other(e));
        }
        let plan = pending.plan.lock().unwrap_or_else(PoisonError::into_inner).take().ok_or_else(|| EngineError::Other("no storyboard".into()))?;
        let built = build(s, plan.clone(), &pending.options)?;
        return Ok(json!({"job": id, "plan": plan, "video": built}));
    }
    Ok(json!({"job": id}))
}

/// Build `plan` with the user's `options` on top.
fn build(s: &mut Session, mut plan: Value, options: &Value) -> Result<Value> {
    if let (Some(pl), Some(o)) = (plan.as_object_mut(), options.as_object()) {
        for (k, v) in o {
            pl.insert(k.clone(), v.clone());
        }
    }
    s.execute("storyboard.build", json!({"plan": plan}))
}

/// Build the plans of finished `storyboard.fromPrompt` jobs (one undo step each) and drop finished or
/// cancelled ones. Called once per UI frame from [`Session::poll_persistence`].
pub fn poll(s: &mut Session) {
    use std::sync::atomic::Ordering;
    let mut i = 0;
    while i < s.storyboard_jobs.len() {
        let job = s.jobs.iter().find(|j| j.id == s.storyboard_jobs[i].job);
        if !job.is_none_or(|j| j.progress.finished.load(Ordering::Relaxed)) {
            i += 1;
            continue;
        }
        let cancelled = job.is_some_and(|j| j.progress.cancel.load(Ordering::Relaxed));
        let pending = s.storyboard_jobs.remove(i);
        let Some(plan) = pending.plan.lock().unwrap_or_else(PoisonError::into_inner).take().filter(|_| !cancelled) else { continue };
        match build(s, plan, &pending.options) {
            Ok(r) => s.toast(format!("Video ready: {} scenes, {:.0} s", r["scenes"].as_array().map_or(0, Vec::len), r["seconds"].as_f64().unwrap_or(0.0))),
            Err(e) => s.error_toast(CMD, format!("Video from prompt: {e}")),
        }
    }
}

pub fn commands() -> Vec<CommandSpec> {
    vec![
        CommandSpec {
            id: CMD,
            label: "Video from Prompt…",
            menu: &["File", "New"],
            shortcut: None,
            params: r#"{"prompt":str,"seconds":f64?,"format":"16:9|9:16|1:1|4:5"?,"voice":str?,"pace":f64?,"captions":bool?,"transition":str?,"music":path?,"music_db":f64?,"wait":bool?} (Claude writes the storyboard in a background job; the video is built when it finishes)"#,
            enabled: always,
            run: from_prompt,
            journal: false,
        },
        CommandSpec {
            id: "storyboard.setApiKey",
            label: "Set Anthropic API Key",
            menu: &[],
            shortcut: None,
            params: r#"{"key":str ("" removes the saved key)}"#,
            enabled: always,
            run: set_api_key,
            journal: false,
        },
    ]
}
