//! 发到飞书之前把 AI 的回答收拾一遍。
//! 规矩里写了“一到两句、不加格式、不贴路径、不说客套话”，但模型（尤其 GPT）常常不照做：
//! 加标题、列表、加粗，贴本机路径，结尾来一句“如果需要，我可以……”。提示词约束不住，这里兜底。

/// `group`：群里的回复——格式全部拍平成一段话，太长只留前几句
pub fn reply(text: &str, group: bool, en: bool) -> String {
    let mut paras: Vec<Vec<String>> = vec![vec![]];
    let mut fence = false;
    for raw in text.lines() {
        let t = raw.trim();
        if t.starts_with("```") || t.starts_with("~~~") {
            fence = !fence;
            continue;
        }
        if t.is_empty() {
            if paras.last().is_some_and(|p| !p.is_empty()) {
                paras.push(vec![]);
            }
            continue;
        }
        // 表格分隔行、分割线
        if !fence && (t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' ')) || t.chars().all(|c| matches!(c, '-' | '*' | '_' | ' '))) {
            continue;
        }
        let Some(line) = (if fence { Some(t.to_string()) } else { plain_line(t, group) }) else { continue };
        let line = strip_paths(&line);
        if !line.trim().is_empty() {
            paras.last_mut().unwrap().push(line);
        }
    }
    let paras: Vec<String> = paras
        .into_iter()
        .filter(|p| !p.is_empty())
        .map(|p| if group { join_lines(&p) } else { p.join("\n") })
        .collect();
    let mut s = paras.join(if group { "\n" } else { "\n\n" });
    s = strip_filler_head(&s);
    s = strip_offer_tail(&s);
    if group {
        s = cap(&s, en);
    }
    if s.trim().is_empty() { text.trim().to_string() } else { s.trim().to_string() }
}

/// 一行去掉 markdown 标记：标题、引用、加粗、行内代码；群里连列表符号也去掉，标题（“结论”“原因”这种小标签）整行不要
fn plain_line(t: &str, group: bool) -> Option<String> {
    let mut t = t.trim_start_matches('>').trim_start();
    let h = t.trim_start_matches('#');
    if h.len() < t.len() && (h.is_empty() || h.starts_with(' ')) {
        if group {
            return None;
        }
        t = h.trim_start();
    }
    if group {
        t = strip_bullet(t);
        if t.starts_with('|') {
            // 表格行：单元格用“，”连起来
            let cells: Vec<&str> = t.split('|').map(str::trim).filter(|c| !c.is_empty()).collect();
            return Some(cells.join("，").replace("**", "").replace("__", "").replace('`', ""));
        }
    }
    Some(t.replace("**", "").replace("__", "").replace('`', ""))
}

fn strip_bullet(t: &str) -> &str {
    for m in ["- ", "* ", "+ ", "• ", "· "] {
        if let Some(r) = t.strip_prefix(m) {
            return r.trim_start();
        }
    }
    // 1. / 1、/ 1)
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 && digits <= 2 {
        let rest = &t[digits..];
        for m in [". ", "、", ") ", "）"] {
            if let Some(r) = rest.strip_prefix(m) {
                return r.trim_start();
            }
        }
    }
    t
}

fn ends_sentence(s: &str) -> bool {
    s.ends_with(['。', '！', '？', '；', '：', '，', '、', '.', '!', '?', ';', ':', ','])
}

/// 群里：一段里的几行连成一句话（列表项之间用分号）
fn join_lines(lines: &[String]) -> String {
    let mut out = String::new();
    for l in lines {
        let l = l.trim();
        if out.is_empty() {
            out.push_str(l);
            continue;
        }
        let ascii = out.chars().last().is_some_and(|c| c.is_ascii()) && l.chars().next().is_some_and(|c| c.is_ascii());
        if !ends_sentence(&out) {
            out.push_str(if ascii { ";" } else { "；" });
        }
        if ascii {
            out.push(' ');
        }
        out.push_str(l);
    }
    out
}

/// 本机路径只留文件名：群里的人看不到这台电脑，路径没用还暴露目录结构
fn strip_paths(s: &str) -> String {
    const ROOTS: [&str; 10] = ["/home/", "/root/", "/Users/", "/workspace/", "/tmp/", "/opt/", "/mnt/", "/var/", "/etc/", "~/"];
    let stop = |c: char| c.is_whitespace() || "，。；：、）)】]\"'”’<>`*,;".contains(c);
    let mut out = String::new();
    let mut i = 0;
    while i < s.len() {
        let rest = &s[i..];
        let prev = s[..i].chars().last();
        let boundary = prev.is_none_or(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '/' | ':' | '_' | '-' | '\\')));
        let win = rest.len() > 3 && rest.as_bytes()[0].is_ascii_alphabetic() && &rest.as_bytes()[1..3] == b":\\";
        if boundary && (win || ROOTS.iter().any(|r| rest.starts_with(r))) {
            let end = rest.find(stop).unwrap_or(rest.len());
            let path = rest[..end].trim_end_matches(['.', ':']);
            let name = path.trim_end_matches(['/', '\\']).rsplit(['/', '\\']).next().unwrap_or_default();
            out.push_str(name);
            i += path.len();
            continue;
        }
        let c = rest.chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// 开头的“好的，”“收到。”
fn strip_filler_head(s: &str) -> String {
    let t = s.trim_start();
    for p in ["好的", "收到", "明白", "没问题", "当然", "OK", "Ok", "Sure", "Got it", "Certainly"] {
        if let Some(r) = t.strip_prefix(p) {
            if let Some(r) = r.strip_prefix(['，', '。', '！', ',', '.', '!']) {
                let r = r.trim_start();
                if !r.is_empty() {
                    return r.to_string();
                }
            }
        }
    }
    t.to_string()
}

/// 分句：中文句末标点、换行；英文句号后面得跟空白（别切 1.2.3、a.com）
fn sentences(s: &str) -> Vec<&str> {
    let mut out = vec![];
    let mut start = 0;
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        let next = it.peek().map(|&(_, n)| n);
        let cut = match c {
            '。' | '！' | '？' | '\n' => true,
            '.' | '!' | '?' => next.is_none_or(char::is_whitespace),
            _ => false,
        };
        if cut {
            let end = i + c.len_utf8();
            push_sentence(s, &mut out, start, end);
            start = end;
        }
    }
    push_sentence(s, &mut out, start, s.len());
    out
}

/// 只有空白的一截（句号后面的换行）并到上一句里，拼回去时换行还在
fn push_sentence<'a>(s: &'a str, out: &mut Vec<&'a str>, start: usize, end: usize) {
    if start == end {
        return;
    }
    if s[start..end].trim().is_empty() {
        if let Some(last) = out.last_mut() {
            let from = last.as_ptr() as usize - s.as_ptr() as usize;
            *last = &s[from..end];
        }
        return;
    }
    out.push(&s[start..end]);
}

const OFFERS: [&str; 22] = [
    "如果需要", "如需", "需要的话", "需要我", "要不要我", "要我", "如果你需要", "如果您需要", "如有需要", "如有", "有需要", "有问题随时", "希望", "我可以继续",
    "我也可以", "随时告诉我", "If you", "If you'd", "Let me know", "Want me to", "Hope ", "I can also",
];

/// 结尾的“如果需要，我可以……”“希望对你有帮助”
fn strip_offer_tail(s: &str) -> String {
    let mut ss = sentences(s);
    while ss.len() > 1 && ss.last().is_some_and(|l| OFFERS.iter().any(|p| l.trim_start().starts_with(p))) {
        ss.pop();
    }
    ss.concat().trim_end().to_string()
}

/// 群里最多三句、约 160 字；超了留前面的（规矩要求结论在前），后面提示去看看里看
fn cap(s: &str, en: bool) -> String {
    const MAX_SENT: usize = 3;
    const MAX_CHARS: usize = 160;
    const HARD: usize = 300;
    let ss = sentences(s);
    let total: usize = s.chars().count();
    if ss.len() <= MAX_SENT && total <= MAX_CHARS {
        return s.to_string();
    }
    let mut kept = String::new();
    for (n, x) in ss.iter().enumerate() {
        if n >= MAX_SENT || (n > 0 && kept.chars().count() + x.chars().count() > MAX_CHARS) {
            break;
        }
        kept.push_str(x);
    }
    let mut kept = kept.trim_end().to_string();
    if kept.chars().count() > HARD {
        kept = kept.chars().take(HARD).collect::<String>() + "…";
    }
    if kept.chars().count() >= total {
        return kept;
    }
    kept.push_str(if en { " (details in LookLook)" } else { "（详情在看看里）" });
    kept
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gpt_style_group_reply_flattened() {
        let gpt = "## 结论\n\nAdStudio 负载高是 **nginx worker** 占满 CPU，已重启。\n\n- 日志在 `/var/log/nginx/error.log`\n- 重启后负载降到 `0.8`\n\n如果需要，我可以继续帮你排查根因。";
        let r = reply(gpt, true, false);
        assert_eq!(r, "AdStudio 负载高是 nginx worker 占满 CPU，已重启。\n日志在 error.log；重启后负载降到 0.8");
        assert!(!r.contains("如果需要"));
    }

    #[test]
    fn dm_keeps_lines_but_drops_noise() {
        let r = reply("好的，已经推到 QA。\n- 改了 /workspace/app/src/login.rs\n\n希望对你有帮助！", false, false);
        assert_eq!(r, "已经推到 QA。\n- 改了 login.rs");
    }

    #[test]
    fn group_cap_and_untouched() {
        let ok = "登录页白屏是 CDN 缓存了旧版本，已刷新，@前端 确认一下。";
        assert_eq!(reply(ok, true, false), ok);
        let long = "第一句结论。第二句补充。第三句说明。第四句过程。第五句更多。";
        assert_eq!(reply(long, true, false), "第一句结论。第二句补充。第三句说明。（详情在看看里）");
        assert_eq!(reply("Fixed in 1.2.3. Let me know if you need more.", true, true), "Fixed in 1.2.3.");
    }

    #[test]
    fn paths_and_urls() {
        assert_eq!(strip_paths("见 /home/a/b/c.txt，和 https://x.com/home/y"), "见 c.txt，和 https://x.com/home/y");
        assert_eq!(strip_paths(r"在 C:\Users\me\proj\a.log 里"), "在 a.log 里");
        // 只有一句客套也别发空
        assert_eq!(reply("希望对你有帮助", true, false), "希望对你有帮助");
    }
}
