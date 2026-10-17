use std::sync::Arc;

use serde_json::{Value, json};

use crate::Session;
use crate::storyboard_ai::{plan_from_response, request_body};

/// A Messages API response whose text is `plan`.
fn reply(plan: &Value) -> Value {
    json!({"stop_reason": "end_turn", "content": [{"type": "thinking", "thinking": ""}, {"type": "text", "text": plan.to_string()}]})
}

fn plan() -> Value {
    json!({
        "name": "Coffee",
        "style": {"title_color": "#ffffff", "body_color": "#ffe8c2"},
        "scenes": [
            {"background": "#3b2418", "title": "Better coffee", "body": "", "narration": "Want better coffee at home?", "animation": "fadeUp"},
            {"background": "not a colour", "title": "Grind fresh", "body": "Right before brewing", "narration": "Grind your beans fresh.", "animation": "pop"}
        ]
    })
}

#[test]
fn request_asks_for_a_strict_plan() {
    let b = request_body("a video about coffee", Some(20.0), "9:16");
    assert_eq!(b["model"], "claude-opus-5-5");
    assert_eq!(b["output_config"]["format"]["type"], "json_schema");
    assert_eq!(b["output_config"]["format"]["schema"]["additionalProperties"], false);
    assert_eq!(b["fallbacks"], "default");
    let msg = b["messages"][0]["content"].as_str().unwrap();
    assert!(msg.contains("about 20 seconds") && msg.contains("vertical 9:16"), "{msg}");
}

#[test]
fn reads_and_cleans_the_plan() {
    let p = plan_from_response(&reply(&plan())).unwrap();
    assert_eq!(p["scenes"].as_array().unwrap().len(), 2);
    assert!(p["scenes"][1].get("background").is_none(), "a bad colour is dropped, not fatal");
    let e = plan_from_response(&json!({"stop_reason": "refusal", "stop_details": {"explanation": "no"}, "content": []})).unwrap_err();
    assert!(e.contains("declined"), "{e}");
    let e = plan_from_response(&json!({"type": "error", "error": {"message": "invalid x-api-key"}})).unwrap_err();
    assert!(e.contains("invalid x-api-key"), "{e}");
    let e = plan_from_response(&json!({"stop_reason": "max_tokens", "content": []})).unwrap_err();
    assert!(e.contains("cut off"), "{e}");
}

#[test]
fn builds_the_video_from_a_prompt() {
    let mut s = Session::default();
    s.execute("file.newProject", json!({"name": "T"})).unwrap();
    let seen = Arc::new(std::sync::Mutex::new(Value::Null));
    let seen2 = seen.clone();
    s.storyboard_planner = Some(Arc::new(move |body: &Value| {
        *seen2.lock().unwrap() = body.clone();
        Ok(reply(&plan()))
    }));
    // background: built by poll when the job finishes
    let r = s.execute("storyboard.fromPrompt", json!({"prompt": "coffee tips", "format": "9:16", "captions": false})).unwrap();
    assert!(r["job"].as_u64().is_some());
    let t0 = std::time::Instant::now();
    while !s.storyboard_jobs.is_empty() {
        s.poll_persistence();
        assert!(t0.elapsed().as_secs() < 30);
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
    let q = s.active_sequence().expect("a new sequence");
    assert_eq!((q.settings.width, q.settings.height), (1080, 1920), "the user's format wins");
    assert_eq!(q.video_tracks[1].items.len(), 2);
    assert!(q.caption_tracks.is_empty(), "the user's captions choice wins");
    assert!(seen.lock().unwrap()["messages"][0]["content"].as_str().unwrap().contains("coffee tips"));

    // wait: true returns the plan and the video
    let r = s.execute("storyboard.fromPrompt", json!({"prompt": "coffee tips", "wait": true})).unwrap();
    assert_eq!(r["plan"]["name"], "Coffee");
    assert_eq!(r["video"]["scenes"].as_array().unwrap().len(), 2);
}

#[test]
fn errors_change_nothing() {
    let mut s = Session::default();
    s.execute("file.newProject", json!({"name": "T"})).unwrap();
    assert!(s.execute("storyboard.fromPrompt", json!({"prompt": "  "})).is_err());
    s.storyboard_planner = Some(Arc::new(|_: &Value| Err("network down".to_string())));
    let rev = s.revision;
    let e = s.execute("storyboard.fromPrompt", json!({"prompt": "x", "wait": true})).unwrap_err();
    assert!(e.to_string().contains("network down"), "{e}");
    assert_eq!(s.revision, rev);
    assert!(s.storyboard_jobs.is_empty());
}
