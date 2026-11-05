use std::sync::Mutex;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::term::Config;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::vte::ansi::Processor;

use super::*;

/// A terminal whose APC strings reach a `Graphics` the test can still inspect, fed through the
/// real (patched) parser.
struct Rig {
    term: Term<VoidListener>,
    parser: Processor,
    graphics: Arc<Mutex<Graphics>>,
    replies: Arc<Mutex<Vec<String>>>,
    shared: Arc<Shared>,
}

impl Rig {
    fn new(columns: usize, lines: usize) -> Self {
        let cfg = Config {
            scrolling_history: 100,
            ..Default::default()
        };
        let mut term = Term::new(cfg, &TermSize::new(columns, lines), VoidListener);
        let shared = Arc::new(Shared::default());
        shared.enabled.store(true, Ordering::Relaxed);
        shared.cell_width.store(10, Ordering::Relaxed);
        shared.cell_height.store(20, Ordering::Relaxed);
        let replies = Arc::new(Mutex::new(Vec::new()));
        let sink = replies.clone();
        let graphics = Arc::new(Mutex::new(Graphics::new(
            shared.clone(),
            Box::new(move |r| sink.lock().unwrap().push(r)),
        )));
        let g = graphics.clone();
        term.set_apc_handler(Some(Box::new(move |t, d| {
            g.lock().unwrap().handle(t, d);
        })));
        Self {
            term,
            parser: Processor::new(),
            graphics,
            replies,
            shared,
        }
    }

    fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    fn apc(&mut self, control: &str, payload: &[u8]) {
        let body = BASE64.encode(payload);
        self.feed(format!("\x1b_G{control};{body}\x1b\\").as_bytes());
    }

    fn replies(&self) -> Vec<String> {
        std::mem::take(&mut *self.replies.lock().unwrap())
    }

    fn at(&self, line: i32, col: usize) -> &[GraphicCell] {
        self.term.grid()[Line(line)][Column(col)].graphics()
    }

    fn cursor(&self) -> (i32, usize) {
        let p = self.term.grid().cursor.point;
        (p.line.0, p.column.0)
    }

    fn images(&self) -> usize {
        self.graphics.lock().unwrap().images.len()
    }
}

fn rgb(w: usize, h: usize) -> Vec<u8> {
    vec![200; w * h * 3]
}

#[test]
fn the_yazi_probe_is_answered_and_shared_memory_is_refused() {
    let mut rig = Rig::new(20, 5);
    // yazi's RequestKgp and RequestKgpShm.
    rig.feed(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
    rig.feed(b"\x1b_Gi=32,s=1,v=1,a=q,t=s,f=24;Y29wYWQ\x1b\\");
    assert_eq!(
        rig.replies(),
        vec![
            "\x1b_Gi=31;OK\x1b\\".to_string(),
            "\x1b_Gi=32;ENOTSUPPORTED:only direct transmission (t=d)\x1b\\".to_string(),
        ]
    );
    assert_eq!(rig.images(), 0);
}

#[test]
fn disabled_swallows_everything_without_a_reply() {
    let mut rig = Rig::new(20, 5);
    rig.shared.enabled.store(false, Ordering::Relaxed);
    rig.feed(b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\x");
    assert!(rig.replies().is_empty());
    assert_eq!(rig.term.grid()[Line(0)][Column(0)].c, 'x');
}

#[test]
fn chunked_transmission_then_placement_covers_cells_and_moves_the_cursor() {
    let mut rig = Rig::new(20, 5);
    let pixels = rgb(20, 40); // 2 x 2 cells at 10 x 20
    let b64 = BASE64.encode(&pixels);
    let (a, b) = b64.split_at(1200);
    rig.feed(format!("\x1b_Ga=t,f=24,s=20,v=40,i=7,m=1;{a}\x1b\\").as_bytes());
    rig.feed(format!("\x1b_Gm=0;{b}\x1b\\").as_bytes());
    assert_eq!(rig.replies(), vec!["\x1b_Gi=7;OK\x1b\\".to_string()]);
    rig.feed(b"ab");
    rig.feed(b"\x1b_Ga=p,i=7,q=1\x1b\\");
    assert!(rig.replies().is_empty(), "q=1 suppresses OK");
    for (line, col) in [(0, 2), (0, 3), (1, 2), (1, 3)] {
        let g = &rig.at(line, col)[0];
        assert_eq!(g.dst, [0.0, 0.0, 1.0, 1.0]);
        assert_eq!(g.image_id, 7);
    }
    assert_eq!(rig.at(0, 2)[0].uv, [0.0, 0.0, 0.5, 0.5]);
    assert_eq!(rig.at(1, 3)[0].uv, [0.5, 0.5, 1.0, 1.0]);
    assert!(rig.at(0, 4).is_empty() && rig.at(2, 2).is_empty());
    // Right by the columns, down by rows - 1.
    assert_eq!(rig.cursor(), (1, 4));
}

#[test]
fn a_partial_last_cell_is_drawn_into_part_of_the_cell() {
    let mut rig = Rig::new(20, 5);
    rig.apc("a=T,f=24,s=15,v=20,i=1,C=1", &rgb(15, 20));
    let edge = &rig.at(0, 1)[0];
    assert_eq!(edge.dst, [0.0, 0.0, 0.5, 1.0]);
    assert!((edge.uv[0] - 10.0 / 15.0).abs() < 1e-6 && edge.uv[2] == 1.0);
    assert_eq!(rig.cursor(), (0, 0), "C=1 keeps the cursor");
}

#[test]
fn columns_and_rows_stretch_and_one_of_them_keeps_the_aspect_ratio() {
    let mut rig = Rig::new(20, 6);
    // 1x1 image into 3 x 2 cells: stretched (kitty's renderer, not letterboxed).
    rig.apc("a=T,f=24,s=1,v=1,i=1,c=3,r=2,C=1", &rgb(1, 1));
    assert_eq!(rig.at(1, 2)[0].dst, [0.0, 0.0, 1.0, 1.0]);
    assert!(rig.at(2, 0).is_empty() && rig.at(0, 3).is_empty());
    // 40x20 image, c=2 -> 20 px wide, so 10 px tall: half a row.
    rig.feed(b"\x1b[5;1H");
    rig.apc("a=T,f=24,s=40,v=20,i=2,c=2,C=1", &rgb(40, 20));
    assert_eq!(rig.at(4, 1)[0].dst, [0.0, 0.0, 1.0, 0.5]);
}

#[test]
fn yazi_per_cell_placements_and_erasure() {
    let mut rig = Rig::new(20, 5);
    rig.apc("q=2,a=t,i=9,f=24,s=20,v=20,m=0", &rgb(20, 20));
    // KgpOld: CUP, then one 1x1 placement per cell with a source rectangle.
    for (n, (x, y)) in [(1, (5, 2)), (2, (6, 2))] {
        let left = (n - 1) * 10;
        rig.feed(
            format!(
                "\x1b[{};{}H\x1b_Gq=2,a=p,i=9,p={n},x={left},y=0,w=10,h=20,c=1,r=1,C=1\x1b\\",
                y + 1,
                x + 1
            )
            .as_bytes(),
        );
    }
    assert_eq!(rig.at(2, 5)[0].uv, [0.0, 0.0, 0.5, 1.0]);
    assert_eq!(rig.at(2, 6)[0].uv, [0.5, 0.0, 1.0, 1.0]);
    assert_eq!(rig.cursor(), (2, 6));
    rig.feed(b"\x1b_Gq=2,a=d,d=i,i=9,p=2\x1b\\");
    assert!(rig.at(2, 6).is_empty() && !rig.at(2, 5).is_empty());
    rig.feed(b"\x1b_Gq=2,a=d,d=I,i=9\x1b\\");
    assert!(rig.at(2, 5).is_empty());
    assert_eq!(rig.images(), 0, "uppercase frees an image nothing shows");
    assert!(rig.replies().is_empty(), "q=2 everywhere");
}

#[test]
fn replacing_a_named_placement_moves_it() {
    let mut rig = Rig::new(20, 5);
    rig.apc("a=t,i=3,f=24,s=10,v=20,q=2", &rgb(10, 20));
    rig.feed(b"\x1b_Ga=p,i=3,p=1,C=1,q=2\x1b\\\x1b[3;4H\x1b_Ga=p,i=3,p=1,C=1,q=2\x1b\\");
    assert!(rig.at(0, 0).is_empty());
    assert_eq!(rig.at(2, 3).len(), 1);
}

#[test]
fn placing_at_the_bottom_scrolls_and_text_carries_the_image_into_history() {
    let mut rig = Rig::new(10, 4);
    rig.feed(b"\x1b[4;1H");
    rig.apc("a=T,f=24,s=10,v=60,i=1,q=2", &rgb(10, 60)); // 3 rows
    // Scrolled up two lines so all three rows fit; cursor on the last image row, one column right.
    assert!(!rig.at(1, 0).is_empty() && !rig.at(3, 0).is_empty());
    assert_eq!(rig.cursor(), (3, 1));
    rig.feed(b"\r\n\n\n");
    assert!(!rig.at(-1, 0).is_empty(), "scrolled into history");
    rig.feed(b"\x1b[1;1Hx");
    assert!(rig.at(0, 0).is_empty());
}

#[test]
fn c1_placement_near_the_bottom_clips_without_scrolling() {
    let mut rig = Rig::new(10, 4);
    rig.feed(b"top\x1b[4;1H");
    rig.apc("a=T,f=24,s=10,v=60,i=1,q=2,C=1", &rgb(10, 60));
    assert_eq!(
        rig.term.grid()[Line(0)][Column(0)].c,
        't',
        "nothing scrolled"
    );
    assert!(!rig.at(3, 0).is_empty());
    assert_eq!(rig.cursor(), (3, 0));
}

#[test]
fn a_placement_inside_the_scroll_region_stays_inside_it() {
    let mut rig = Rig::new(10, 8);
    // Region lines 2..=5 (1-based 3..6), cursor on its last line.
    rig.feed(b"\x1b[3;6r\x1b[6;1H");
    rig.apc("a=T,f=24,s=10,v=80,i=1,q=2", &rgb(10, 80)); // 4 rows
    assert!(
        rig.at(6, 0).is_empty() && rig.at(7, 0).is_empty(),
        "never below the region"
    );
    assert!(!rig.at(2, 0).is_empty() && !rig.at(5, 0).is_empty());
    assert_eq!(rig.cursor().0, 5);
}

#[test]
fn placements_inside_a_synchronized_update_keep_stream_order() {
    let mut rig = Rig::new(20, 5);
    rig.apc("a=t,i=4,f=24,s=10,v=20,q=2", &rgb(10, 20));
    rig.feed(b"\x1b[?2026h\x1b[2;3H\x1b_Ga=p,i=4,C=1,q=2\x1b\\\x1b[5;9Hzz\x1b[?2026l");
    assert_eq!(rig.at(1, 2).len(), 1);
    assert!(rig.at(4, 8).is_empty());
}

#[test]
fn overlapping_placements_stack_and_uncover() {
    let mut rig = Rig::new(20, 5);
    rig.apc("a=t,i=1,f=24,s=10,v=20,q=2", &rgb(10, 20));
    rig.apc("a=t,i=2,f=24,s=10,v=20,q=2", &rgb(10, 20));
    rig.feed(b"\x1b_Ga=p,i=2,z=5,C=1,q=2\x1b\\\x1b_Ga=p,i=1,C=1,q=2\x1b\\");
    let ids: Vec<u32> = rig.at(0, 0).iter().map(|g| g.image_id).collect();
    assert_eq!(ids, vec![1, 2], "bottom-most first");
    rig.feed(b"\x1b_Ga=d,d=i,i=2,q=2\x1b\\");
    assert_eq!(rig.at(0, 0)[0].image_id, 1);
}

#[test]
fn visible_deletes_spare_history_and_positional_ones_remove_whole_placements() {
    let mut rig = Rig::new(10, 3);
    rig.apc("a=t,i=1,f=24,s=20,v=20,q=2", &rgb(20, 20));
    rig.feed(b"\x1b_Ga=p,i=1,C=1,q=2\x1b\\\n\n\n"); // line 0 -> 2, then one scroll
    assert!(!rig.at(-1, 0).is_empty());
    rig.feed(b"\x1b[1;1H\x1b_Ga=p,i=1,C=1,q=2\x1b\\");
    rig.feed(b"\x1b_Ga=d,d=p,x=2,y=1,q=2\x1b\\");
    assert!(
        rig.at(0, 0).is_empty(),
        "the whole placement under (2,1) went"
    );
    assert!(!rig.at(-1, 0).is_empty(), "history is not visible");
    rig.feed(b"\x1b_Ga=d,d=i,i=1,q=2\x1b\\");
    assert!(rig.at(-1, 0).is_empty(), "an id delete reaches history");
}

#[test]
fn quota_evicts_unreferenced_images_first_then_refuses() {
    let mut rig = Rig::new(10, 3);
    let size = 10 * 20 * 4;
    rig.graphics.lock().unwrap().quota = size * 2;
    rig.apc("a=t,i=1,f=24,s=10,v=20,q=2", &rgb(10, 20));
    rig.apc("a=T,i=2,f=24,s=10,v=20,q=2,C=1", &rgb(10, 20));
    rig.apc("a=t,i=3,f=24,s=10,v=20,q=2", &rgb(10, 20));
    let g = rig.graphics.lock().unwrap();
    assert!(!g.images.contains_key(&1) && g.images.contains_key(&2) && g.images.contains_key(&3));
    assert_eq!(g.live_bytes(), size * 2);
    drop(g);
    rig.apc("a=t,i=5,f=24,s=20,v=30", &rgb(20, 30));
    assert_eq!(
        rig.replies(),
        vec!["\x1b_Gi=5;ENOMEM:image storage quota exceeded\x1b\\".to_string()]
    );
}

#[test]
fn png_and_zlib_and_rgba_premultiply() {
    let mut rig = Rig::new(10, 3);
    let mut png_bytes = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut png_bytes, 2, 1);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut w = enc.write_header().unwrap();
        w.write_image_data(&[255, 0, 0, 255, 255, 255, 255, 0])
            .unwrap();
    }
    rig.apc("a=t,i=1,f=100", &png_bytes);
    let mut z = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut z, &[10, 20, 30, 128]).unwrap();
    rig.apc("a=t,i=2,f=32,s=1,v=1,o=z", &z.finish().unwrap());
    assert_eq!(
        rig.replies(),
        vec![
            "\x1b_Gi=1;OK\x1b\\".to_string(),
            "\x1b_Gi=2;OK\x1b\\".to_string()
        ]
    );
    let g = rig.graphics.lock().unwrap();
    assert_eq!(g.images[&1].image.rgba, vec![255, 0, 0, 255, 0, 0, 0, 0]);
    assert_eq!(g.images[&2].image.rgba, vec![5, 10, 15, 128]);
}

#[test]
fn errors_name_the_image() {
    let mut rig = Rig::new(10, 3);
    rig.feed(b"\x1b_Ga=p,i=44\x1b\\");
    rig.apc("a=t,i=45,f=24,s=2,v=2", &rgb(1, 1));
    rig.feed(b"\x1b_Ga=p,i=44,q=2\x1b\\\x1b_Ga=p\x1b\\");
    assert_eq!(
        rig.replies(),
        vec![
            "\x1b_Gi=44;ENOENT:no such image\x1b\\".to_string(),
            "\x1b_Gi=45;ENODATA:pixel data does not match the size\x1b\\".to_string(),
        ]
    );
}

#[test]
fn image_numbers_get_an_id_assigned_and_reported() {
    let mut rig = Rig::new(10, 3);
    rig.apc("a=t,I=13,f=24,s=1,v=1", &rgb(1, 1));
    let reply = rig.replies().remove(0);
    assert!(reply.starts_with("\x1b_Gi=2147483648,I=13;OK"), "{reply:?}");
    rig.feed(b"\x1b_Ga=p,I=13,C=1,q=2\x1b\\");
    assert_eq!(rig.at(0, 0)[0].image_id, 1 << 31);
}

#[test]
fn uppercase_delete_frees_the_image_even_while_a_snapshot_holds_it() {
    let mut rig = Rig::new(10, 3);
    rig.apc("a=T,i=1,f=24,s=10,v=20,q=2,C=1", &rgb(10, 20));
    let (_, held) = collect(rig.term.grid(), 0, 3, None);
    rig.feed(b"\x1b_Ga=d,d=I,i=1,q=2\x1b\\");
    assert_eq!(rig.images(), 0);
    rig.feed(b"\x1b_Ga=p,i=1\x1b\\");
    assert_eq!(
        rig.replies(),
        vec!["\x1b_Gi=1;ENOENT:no such image\x1b\\".to_string()]
    );
    drop(held);
    assert_eq!(rig.graphics.lock().unwrap().live_bytes(), 0);
}

#[test]
fn huge_spans_are_clamped_not_overflowed() {
    let mut rig = Rig::new(10, 3);
    rig.feed(b"\x1b[1;2H");
    rig.apc("a=T,i=1,f=24,s=1,v=1,c=4294967295,r=1,q=2", &rgb(1, 1));
    rig.apc("a=T,i=2,f=24,s=1,v=1,c=1,r=4294967295,q=2", &rgb(1, 1));
    rig.feed(b"x");
    let p = rig.term.grid().cursor.point;
    assert!(p.column.0 < 10 && p.line.0 < 3);
}

#[test]
fn image_memory_is_returned_when_the_last_cell_lets_go() {
    let mut rig = Rig::new(10, 3);
    rig.apc("a=T,i=1,f=24,s=10,v=20,q=2,C=1", &rgb(10, 20));
    rig.feed(b"\x1b_Ga=d,d=I,i=1,q=2\x1b\\");
    assert_eq!(rig.graphics.lock().unwrap().live_bytes(), 0);
}

#[test]
fn snapshot_runs_merge_per_row_and_sort_by_stacking() {
    let mut rig = Rig::new(10, 4);
    rig.apc("a=t,i=2,f=24,s=30,v=20,q=2", &rgb(30, 20));
    rig.apc("a=t,i=1,f=24,s=10,v=20,q=2", &rgb(10, 20));
    // A wide z=10 image, then a small z=0 one over its second column.
    rig.feed(b"\x1b_Ga=p,i=2,z=10,C=1,q=2\x1b\\\x1b[1;2H\x1b_Ga=p,i=1,C=1,q=2\x1b\\");
    let (runs, images) = collect(rig.term.grid(), 0, 4, None);
    assert_eq!(runs.len(), 2, "the 3-cell image is one run");
    assert_eq!(
        (runs[0].z, runs[1].z),
        (0, 10),
        "z=0 draws first although it sits to the right"
    );
    assert_eq!(runs[1].rect, [0.0, 0.0, 3.0, 1.0]);
    assert_eq!(runs[1].uv, [0.0, 0.0, 1.0, 1.0]);
    assert_eq!(images.len(), 2);
    assert_eq!(images[runs[1].image as usize].width, 30);
}

#[test]
fn snapshot_runs_follow_the_viewport_into_history() {
    let mut rig = Rig::new(10, 2);
    rig.apc("a=T,i=1,f=24,s=10,v=20,q=2,C=1", &rgb(10, 20));
    rig.feed(b"\n\n\n");
    assert!(collect(rig.term.grid(), 0, 2, None).0.is_empty());
    let (runs, _) = collect(rig.term.grid(), 2, 2, None);
    assert_eq!(runs[0].rect[1], 0.0);
}

/// Lock-hold cost of a 4K upload (`cargo test --release -p copad-term -- --ignored --nocapture`).
#[test]
#[ignore]
fn timing_4k_upload() {
    let mut rig = Rig::new(200, 60);
    let pixels = vec![128u8; 3840 * 2160 * 4];
    let b64 = BASE64.encode(&pixels);
    let chunks: Vec<&str> = b64
        .as_bytes()
        .chunks(4096)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    let mut stream = Vec::new();
    for (i, c) in chunks.iter().enumerate() {
        let more = (i + 1 < chunks.len()) as u8;
        if i == 0 {
            stream
                .extend(format!("\x1b_Ga=T,f=32,s=3840,v=2160,i=1,q=2,m={more};{c}\x1b\\").bytes());
        } else {
            stream.extend(format!("\x1b_Gm={more};{c}\x1b\\").bytes());
        }
    }
    let start = std::time::Instant::now();
    for piece in stream.chunks(65536) {
        rig.feed(piece);
    }
    eprintln!(
        "4K RGBA upload + place: {:?} for {} MB of escapes",
        start.elapsed(),
        stream.len() >> 20
    );
    assert!(!rig.at(0, 0).is_empty());
}

#[test]
fn effects_describe_the_scroll_the_cursor_and_the_linefeed() {
    let mut rig = Rig::new(10, 4);
    rig.apc("a=t,i=1,f=24,s=100,v=60,q=2", &rgb(100, 60)); // 10 x 3 cells
    rig.feed(b"\x1b[4;1H");
    let g = rig.graphics.clone();
    // Three rows from the last line: two lines of scroll, the cursor on the last image row, and
    // the ten columns reach the edge, so a linefeed follows.
    let e = g.lock().unwrap().handle(&mut rig.term, b"Ga=p,i=1,q=2");
    assert_eq!(e.scrolled, 2);
    assert_eq!(e.cursor, Some(Point::new(Line(3), Column(0))));
    assert!(e.linefeed);
    let e = g.lock().unwrap().handle(&mut rig.term, b"Ga=p,i=1,q=2,C=1");
    assert_eq!(e, Effects::default());
}

#[test]
fn a_viewport_above_the_retained_history_is_clipped_not_shifted() {
    let mut rig = Rig::new(10, 2);
    rig.apc("a=T,i=1,f=24,s=10,v=20,q=2,C=1", &rgb(10, 20));
    rig.feed(b"\n\n\n");
    let history = rig.term.grid().history_size() as i32;
    // A host scrolled 5 lines further back than this grid keeps.
    let (runs, _) = collect(rig.term.grid(), history + 5, 2, None);
    assert!(runs.is_empty());
    let (runs, _) = collect(rig.term.grid(), 2, 2, None);
    assert_eq!(runs.len(), 1);
}

#[test]
fn the_revision_moves_with_what_is_drawn() {
    let mut rig = Rig::new(10, 2);
    let rev = |rig: &Rig| rig.graphics.lock().unwrap().revision();
    let r0 = rev(&rig);
    rig.apc("a=t,i=1,f=24,s=10,v=20,q=2", &rgb(10, 20));
    assert_eq!(rev(&rig), r0, "a transmission draws nothing");
    rig.feed(b"\x1b_Ga=p,i=1,C=1,q=2\x1b\\");
    let r1 = rev(&rig);
    assert!(r1 > r0);
    rig.feed(b"\x1b_Ga=d,d=a,q=2\x1b\\");
    assert!(rev(&rig) > r1);
}

#[test]
fn per_cell_placements_of_one_image_merge_into_one_run_per_row() {
    let mut rig = Rig::new(10, 4);
    rig.apc("a=t,i=9,f=24,s=30,v=40,q=2", &rgb(30, 40)); // 3 x 2 cells
    // yazi's KgpOld: one 1x1 placement per cell, each cut from the image.
    for y in 0..2u32 {
        for x in 0..3u32 {
            rig.feed(
                format!(
                    "\x1b[{};{}H\x1b_Ga=p,i=9,p={},x={},y={},w=10,h=20,c=1,r=1,C=1,q=2\x1b\\",
                    y + 1,
                    x + 1,
                    y * 3 + x + 1,
                    x * 10,
                    y * 20
                )
                .as_bytes(),
            );
        }
    }
    let (runs, _) = collect(rig.term.grid(), 0, 4, None);
    assert_eq!(runs.len(), 2);
    assert_eq!(runs[0].rect, [0.0, 0.0, 3.0, 1.0]);
    assert_eq!(runs[0].uv, [0.0, 0.0, 1.0, 0.5]);
}

fn placeholders(rig: &Rig) -> (std::collections::HashMap<(u32, u32), Virtual>, (u32, u32)) {
    let v = rig.graphics.lock().unwrap().virtuals();
    let map = v.lock().unwrap().clone();
    (map, rig.shared.cell())
}

#[test]
fn unicode_placeholders_show_their_virtual_placement() {
    let mut rig = Rig::new(10, 4);
    // 20x40 image, virtual placement 2 x 2 cells (10x20 px each): exactly one cell per quarter.
    rig.apc("a=T,U=1,i=42,c=2,r=2,f=24,s=20,v=40,q=2", &rgb(20, 40));
    assert!(
        collect(rig.term.grid(), 0, 4, None).0.is_empty(),
        "a virtual placement draws nothing itself"
    );
    // kitty's example: explicit row/col on the first cell, the second inherits the column.
    rig.feed("\x1b[38;5;42m\u{10EEEE}\u{305}\u{305}\u{10EEEE}\u{305}\x1b[39m\r\n".as_bytes());
    rig.feed("\x1b[38;5;42m\u{10EEEE}\u{30D}\u{305}\u{10EEEE}\x1b[39m".as_bytes());
    let (map, cell) = placeholders(&rig);
    let ph = Placeholders {
        virtuals: &map,
        cell,
    };
    let (runs, images) = collect(rig.term.grid(), 0, 4, Some(&ph));
    assert_eq!(runs.len(), 2, "one run per row");
    assert_eq!(runs[0].rect, [0.0, 0.0, 2.0, 1.0]);
    // The 20x40 image fit into a 20x40 box: rows split it in halves.
    assert_eq!(runs[0].uv, [0.0, 0.0, 1.0, 0.5]);
    assert_eq!(runs[1].uv, [0.0, 0.5, 1.0, 1.0]);
    assert_eq!(images[0].width, 20);
}

#[test]
fn placeholders_letterbox_and_a_missing_placement_shows_nothing() {
    let mut rig = Rig::new(10, 4);
    // 40x20 image into a 2x2-cell box (20x40 px): fit to width, centred vertically.
    rig.apc("a=T,U=1,i=7,c=2,r=2,f=24,s=40,v=20,q=2", &rgb(40, 20));
    rig.feed("\x1b[38;2;0;0;7m\u{10EEEE}\u{305}\u{305}\u{10EEEE}\u{305}\u{30D}\x1b[39m".as_bytes());
    rig.feed("\x1b[2;1H\x1b[38;2;0;0;9m\u{10EEEE}\u{305}\u{305}\x1b[39m".as_bytes());
    let (map, cell) = placeholders(&rig);
    let ph = Placeholders {
        virtuals: &map,
        cell,
    };
    let (runs, _) = collect(rig.term.grid(), 0, 4, Some(&ph));
    assert_eq!(runs.len(), 1, "image 9 has no placement");
    // The image is 10 px tall in a 40 px box: rows 15..25 — the bottom 5 px of row 0.
    assert_eq!(runs[0].rect, [0.0, 0.75, 2.0, 1.0]);
    // Deleting the image takes its virtual placement with it.
    rig.feed(b"\x1b_Ga=d,d=I,i=7,q=2\x1b\\");
    assert!(placeholders(&rig).0.is_empty());
}

#[test]
fn evicting_an_image_takes_its_virtual_placements_with_it() {
    let mut rig = Rig::new(10, 3);
    let size = 10 * 20 * 4;
    rig.graphics.lock().unwrap().quota = size;
    // Image 1 is held by a virtual placement only; image 2 needs its room.
    rig.apc("a=T,U=1,i=1,f=24,s=10,v=20,q=2", &rgb(10, 20));
    rig.apc("a=t,i=2,f=24,s=10,v=20,q=2", &rgb(10, 20));
    assert!(
        placeholders(&rig).0.is_empty(),
        "image 1's virtual placement went with it"
    );
    assert_eq!(
        rig.graphics.lock().unwrap().live_bytes(),
        size,
        "its bytes were freed for image 2"
    );
}
