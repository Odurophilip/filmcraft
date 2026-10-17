use serde_json::json;

use crate::Session;

fn plan() -> serde_json::Value {
    json!({
        "name": "Launch",
        "format": "16:9",
        "style": {"title_color": "#ffcc00"},
        "scenes": [
            {"background": "#1d3557", "title": "Make videos from nothing", "body": "Type a brief. Get a video.",
             "narration": "Make a video from nothing. Type a brief, and get a finished video.", "animation": "fadeUp"},
            {"background": {"color": "#e63946"}, "title": "Edit everything", "seconds": 3, "animation": "pop"},
            {"title": "Try it today", "narration": "Try it today.", "animation": "slideLeft"}
        ]
    })
}

#[test]
fn builds_an_editable_sequence_in_one_undo_step() {
    let mut s = Session::default();
    s.execute("file.newProject", json!({"name": "T"})).unwrap();
    let undo_before = s.history.undo.len();
    let r = s.execute("storyboard.build", json!({"plan": plan()})).unwrap();
    assert_eq!(r["width"], 1920);
    let scenes = r["scenes"].as_array().unwrap();
    assert_eq!(scenes.len(), 3);
    assert_eq!(scenes[1]["seconds"], 3.0, "an explicit length without narration");
    assert!(scenes[0]["seconds"].as_f64().unwrap() > 2.0, "lasts as long as its narration: {}", scenes[0]);
    assert!(scenes[0]["captions"].as_u64().unwrap() >= 2);
    assert!(r["warnings"].as_array().unwrap().is_empty(), "{}", r["warnings"]);

    let q = s.active_sequence().unwrap();
    assert_eq!(q.video_tracks[0].items.len(), 3, "a background per scene on V1");
    assert_eq!(q.video_tracks[1].items.len(), 3, "a text clip per scene on V2");
    assert_eq!(q.audio_tracks[0].items.len(), 2, "narrations on A1");
    assert_eq!(q.duration().0, r["duration"].as_i64().unwrap());
    // scenes are back to back
    let starts: Vec<i64> = scenes.iter().map(|x| x["start"].as_i64().unwrap()).collect();
    assert_eq!(starts[1], starts[0] + scenes[0]["duration"].as_i64().unwrap());

    // one undo step removes it all
    assert_eq!(s.history.undo.len(), undo_before + 1);
    s.execute("edit.undo", json!({})).unwrap();
    assert!(s.project.sequences().count() == 0 || s.active_sequence().is_none_or(|q| q.duration().0 == 0));
}

#[test]
fn a_bad_plan_changes_nothing() {
    let mut s = Session::default();
    s.execute("file.newProject", json!({"name": "T"})).unwrap();
    let rev = s.revision;
    for (plan, want) in [
        (json!({"scenes": []}), "1 to"),
        (json!({"scenes": [{"background": "red"}]}), "colour"),
        (json!({"scenes": [{"background": {"generate": "a beach"}}]}), "generated footage"),
        (json!({"scenes": [{"animation": "spin"}]}), "animation"),
        (json!({"format": "3:2", "scenes": [{}]}), "format"),
        (json!({"scenes": [{"titel": "typo"}]}), "bad plan"),
    ] {
        let e = s.execute("storyboard.build", json!({"plan": plan})).unwrap_err().to_string();
        assert!(e.contains(want), "{e}");
    }
    assert_eq!(s.revision, rev);
}

#[test]
fn vertical_format_and_no_captions() {
    let mut s = Session::default();
    s.execute("file.newProject", json!({"name": "T"})).unwrap();
    let r = s
        .execute("storyboard.build", json!({"format": "9:16", "captions": false, "transition": "none", "scenes": [{"title": "Hi", "narration": "Hello."}]}))
        .unwrap();
    assert_eq!((r["width"].as_u64(), r["height"].as_u64()), (Some(1080), Some(1920)));
    assert!(s.active_sequence().unwrap().caption_tracks.is_empty());
}
