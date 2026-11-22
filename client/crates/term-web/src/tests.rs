use super::*;
use term_core::{AlacrittyCore, CoreConfig, TerminalCore};
use term_proto::{encode_raw_packet, DeltaEncoder};

/// 一个“主机”：真实的内核 + 差量编码器，按帧把消息编成解压后的字节流（和 termd 发出、JS 解压后交给 feed 的一样）
struct Host {
    core: AlacrittyCore,
    enc: DeltaEncoder,
}

impl Host {
    fn new(cols: u16, rows: u16) -> Self {
        Host {
            core: AlacrittyCore::new(CoreConfig { cols, rows, ..CoreConfig::default() }).unwrap(),
            enc: DeltaEncoder::new(),
        }
    }

    /// 喂一段输出，返回这一帧（加上滚出去的回滚行）的字节
    fn frame(&mut self, bytes: &[u8], input_seq: u64) -> Vec<u8> {
        self.core.feed_bytes(bytes);
        let mut out = Vec::new();
        let (epoch, rows) = self.core.take_new_scrollback();
        if !rows.is_empty() {
            out.extend(encode_raw_packet(&ServerMsg::Scrolled { epoch, rows }).unwrap());
        }
        let snap = self.core.snapshot();
        out.extend(encode_raw_packet(&self.enc.encode_frame(&snap, input_seq)).unwrap());
        out
    }
}

fn parse(json: &str) -> Vec<String> {
    // 只做断言用：按顶层对象粗分（够用：测试里只看 "t" 字段和几个关键子串）
    let mut items = Vec::new();
    let (mut depth, mut start, mut in_str, mut esc) = (0i32, 0usize, false, false);
    for (i, ch) in json.char_indices() {
        if in_str {
            match (esc, ch) {
                (true, _) => esc = false,
                (false, '\\') => esc = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match ch {
            '"' => in_str = true,
            '{' => {
                if depth == 1 {
                    start = i;
                }
                depth += 1;
            }
            '}' => {
                depth -= 1;
                if depth == 1 {
                    items.push(json[start..=i].to_string());
                }
            }
            '[' => depth += 1,
            ']' => depth -= 1,
            _ => {}
        }
    }
    items
}

/// 客户端还原出的每一行文字和主机内核里的一样
fn assert_same_text(host: &Host, client: &TermClient) {
    let snap = host.core.snapshot();
    assert_eq!((client.cols(), client.rows()), (snap.cols, snap.rows));
    for r in 0..snap.rows {
        assert_eq!(client.row_text(r), row_plain_text(&snap.lines[r as usize].cells), "row {r}");
    }
}

#[test]
fn snapshot_then_deltas_track_host_screen() {
    let mut host = Host::new(40, 8);
    let mut client = TermClient::new();
    let ev = client.feed(&host.frame(b"hello\r\nworld", 0));
    let items = parse(&ev);
    assert_eq!(items.len(), 1);
    assert!(items[0].contains("\"t\":\"screen\"") && items[0].contains("\"full\":true"));
    assert!(items[0].contains("\"dirty\":[0,1,2,3,4,5,6,7]"));
    assert_same_text(&host, &client);
    assert_eq!(client.row(0), r#"[{"t":"hello"}]"#);

    // 改一行：差量里只有这一行
    let ev = client.feed(&host.frame(b"\r\x1b[31mred\x1b[0m!", 0));
    assert!(ev.contains("\"full\":false") && ev.contains("\"dirty\":[1]"), "{ev}");
    assert_eq!(client.row(1), r#"[{"t":"red","f":1},{"t":"!d"}]"#);
    assert_same_text(&host, &client);
}

#[test]
fn output_scrolling_reports_shift_and_scrollback_rows() {
    let mut host = Host::new(20, 8);
    let mut client = TermClient::new();
    client.feed(&host.frame(b"a\r\nb\r\nc\r\nd\r\ne\r\nf\r\ng\r\nh", 0));
    // 再输出 3 行：屏幕整块上移，滚出去的 a b c 进回滚区
    let ev = client.feed(&host.frame(b"\r\ni\r\nj\r\nk", 0));
    let items = parse(&ev);
    let scrolled = items.iter().find(|i| i.contains("\"t\":\"scrolled\"")).unwrap_or_else(|| panic!("{ev}"));
    for t in ["{\"t\":\"a\"}", "{\"t\":\"b\"}", "{\"t\":\"c\"}"] {
        assert!(scrolled.contains(t), "{scrolled}");
    }
    let screen = items.iter().find(|i| i.contains("\"t\":\"screen\"")).expect("screen");
    assert!(screen.contains("\"shifts\":[[0,8,3]]"), "{screen}");
    // 移动后空出来的最后 3 行要重画
    assert!(screen.contains("\"dirty\":[5,6,7]"), "{screen}");
    assert_same_text(&host, &client);
    assert_eq!(client.row_text(0), "d");
    assert_eq!(client.row_text(7), "k");
}

#[test]
fn wide_and_non_ascii_cells_get_fixed_width_boxes() {
    let mut host = Host::new(20, 2);
    let mut client = TermClient::new();
    client.feed(&host.frame("ab中文─x😀".as_bytes(), 0));
    // ASCII 连成一串；中文、emoji 每个字一个 2 格盒子；制表线（单宽非 ASCII）1 格盒子
    assert_eq!(
        client.row(0),
        r#"[{"t":"ab"},{"w":2,"c":["中","文"]},{"w":1,"c":["─"]},{"t":"x"},{"w":2,"c":["😀"]}]"#
    );
    assert_eq!(client.row_text(0), "ab中文─x😀");
}

#[test]
fn styles_colors_and_links() {
    let mut host = Host::new(30, 2);
    let mut client = TermClient::new();
    client.feed(&host.frame(b"\x1b[1;38;2;1;2;3;48;5;200mX\x1b[0m\x1b]8;;https://e.x/\"q\x1b\\L\x1b]8;;\x1b\\", 0));
    let row = client.row(0);
    let rgb = RGB_BASE + 0x010203;
    assert!(row.contains(&format!("{{\"t\":\"X\",\"f\":{rgb},\"b\":200,\"a\":1}}")), "{row}");
    assert!(row.contains(r#""l":"https://e.x/\"q""#), "{row}");
}

#[test]
fn input_kept_until_acked_and_resent() {
    let mut host = Host::new(10, 2);
    let mut client = TermClient::new();
    client.feed(&host.frame(b"", 0));
    let a = client.input(b"a");
    let _b = client.input(b"b");
    assert_eq!(client.pending_inputs(), 2);
    assert_eq!(client.pending_input(0), a);
    // termd 处理到第 1 条
    client.feed(&host.frame(b"a", 1));
    assert_eq!(client.pending_inputs(), 1);
    match term_proto::decode_client_msg(&client.pending_input(0)).unwrap() {
        ClientMsg::Input { seq, data } => assert_eq!((seq, data), (2, b"b".to_vec())),
        other => panic!("{other:?}"),
    }
    // 续接：Hello 带着当前帧号和已确认的输入序号
    client.reset_stream(true);
    match term_proto::decode_client_msg(&client.hello(10, 2, Some("c1".into()), true)).unwrap() {
        ClientMsg::Hello { last_frame, last_input_seq, client_id, .. } => {
            assert_eq!(last_frame, Some(client.frame() as u64));
            assert_eq!(last_input_seq, 1);
            assert_eq!(client_id.as_deref(), Some("c1"));
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn frame_gap_asks_for_resync() {
    let mut host = Host::new(10, 2);
    let mut client = TermClient::new();
    client.feed(&host.frame(b"x", 0));
    let _lost = host.frame(b"y", 0); // 这一帧没送到
    let ev = client.feed(&host.frame(b"z", 0));
    assert!(ev.contains("\"t\":\"resync\""), "{ev}");
}

#[test]
fn packets_split_across_feeds() {
    let mut host = Host::new(10, 2);
    let mut client = TermClient::new();
    let bytes = host.frame(b"split", 0);
    let (a, b) = bytes.split_at(bytes.len() / 2);
    assert_eq!(client.feed(a), "[]");
    assert!(client.feed(b).contains("\"t\":\"screen\""));
    assert_eq!(client.row_text(0), "split");
}

#[test]
fn json_strings_are_escaped() {
    let mut s = String::new();
    push_json_str(&mut s, "a\"b\\c\n\u{1}\u{2028}");
    assert_eq!(s, "\"a\\\"b\\\\c\\n\\u0001\\u2028\"");
}
