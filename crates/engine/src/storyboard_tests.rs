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
    assert_eq!(q.video_tracks[2].items.len(), 3, "a title per scene on V3");
    assert_eq!(q.video_tracks[3].items.len(), 1, "the one body line on V4");
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

#[test]
fn top_layout_caption_text_and_zoom() {
    let mut s = Session::default();
    s.execute("file.newProject", json!({"name": "T"})).unwrap();
    let r = s
        .execute("storyboard.build", json!({"scenes": [{"title": "Hi", "layout": "top", "narration": "Path Axis is here.", "caption": "Pathaxis is here."}]}))
        .unwrap();
    let q = s.active_sequence().unwrap();
    let words: Vec<String> = q.caption_tracks[0].captions.iter().map(|c| c.text.clone()).collect();
    assert_eq!(words.join(" "), "Pathaxis is here.", "captions read the caption text, not what the voice says");
    assert!(r["scenes"][0]["text"].as_u64().is_some());
    for (bad, want) in [(json!({"layout": "side"}), "layout"), (json!({"background": {"color": "#000000", "zoom": 300}}), "zoom")] {
        let e = s.execute("storyboard.build", json!({"scenes": [bad]})).unwrap_err().to_string();
        assert!(e.contains(want), "{e}");
    }
}

#[test]
fn pictures_highlights_and_per_scene_transitions() {
    let dir = std::env::temp_dir().join(format!("fc-storyboard-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let png = dir.join("shot.png");
    // a 1×1 PNG is enough to import and place
    std::fs::write(
        &png,
        [
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0x0D, 0x49, 0x48, 0x44, 0x52, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0, 0x1F, 0x15, 0xC4,
            0x89, 0, 0, 0, 0x0D, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9C, 0x63, 0xF8, 0xCF, 0xC0, 0xF0, 0x1F, 0, 0x05, 0x00, 0x01, 0xFF, 0x89, 0x99, 0x3D, 0x1D, 0,
            0, 0, 0, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ],
    )
    .unwrap();
    let mut s = Session::default();
    s.execute("file.newProject", json!({"name": "T"})).unwrap();
    let shot = png.to_string_lossy();
    let r = s
        .execute(
            "storyboard.build",
            json!({"style": {"accent": "#c96442"}, "scenes": [
                {"title": "One", "seconds": 3},
                {"title": "Two", "body": "later", "layout": "top", "seconds": 4, "transition": "push",
                 "image": {"media": shot, "enter": "slideUp"},
                 "highlights": [{"x": 100, "y": 100, "w": 200, "h": 80, "at": 1.0}]}
            ]}),
        )
        .unwrap();
    assert!(r["warnings"].as_array().unwrap().is_empty(), "{}", r["warnings"]);
    assert!(r["scenes"][1]["image"].as_u64().is_some());
    let q = s.active_sequence().unwrap();
    assert_eq!(q.video_tracks[1].items.len(), 1, "the picture on V2");
    assert_eq!(q.video_tracks[4].items.len(), 1, "the highlight ring on V5");
    let ring = &q.video_tracks[4].items[0];
    assert!(ring.start.0 > q.video_tracks[0].items[1].start.0, "the ring comes in after the scene starts");
    let e = s.execute("storyboard.build", json!({"scenes": [{"image": {"media": "/nope.png"}}]})).unwrap_err().to_string();
    assert!(e.contains("no such image"), "{e}");
}
