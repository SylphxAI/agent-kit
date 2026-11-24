use super::super::{
    edit_message_text, escape_rich_markdown_dollars, markdown_to_telegram_html,
    preserve_rich_markdown_line_breaks, rich_markdown, send_message,
};

#[test]
fn gfm_markdown_table_and_html_br_render_as_rich_telegram_html_cards() {
    let md = "Intro:\n\n| # | Owner | Model | Reason |\n|---|---|---|---|\n| **1** | **Team A** (`Primary`) | **`Model A (high)`**<br>*(alt: `Model B`)* | Rank **#1** |\n";
    let html = markdown_to_telegram_html(md);
    assert!(!html.contains("|---|"), "{html}");
    assert!(!html.contains("&lt;br&gt;"), "{html}");
    assert!(
        html.contains("<blockquote><b>1</b> · <b>Team A</b> (<code>Primary</code>)"),
        "{html}"
    );
    assert!(html.contains("• <b>Model</b>: <b><code>Model A (high)</code></b> / <i>(alt: <code>Model B</code>)</i>"), "{html}");
    assert!(
        html.contains("• <b>Reason</b>: Rank <b>#1</b></blockquote>"),
        "{html}"
    );
}

#[test]
fn single_newlines_become_hard_breaks_under_headings_and_bullets() {
    assert_eq!(
        preserve_rich_markdown_line_breaks("Heading\n• Bullet 1\n• Bullet 2"),
        "Heading  \n• Bullet 1  \n• Bullet 2"
    );
    assert_eq!(
        preserve_rich_markdown_line_breaks("Heading\n- Bullet 1\n- Bullet 2\n"),
        "Heading  \n- Bullet 1  \n- Bullet 2\n"
    );
    assert_eq!(
        preserve_rich_markdown_line_breaks("## Status\nline one\nline two"),
        "## Status\nline one  \nline two"
    );
}

#[test]
fn multi_line_prose_keeps_breaks_and_paragraphs_stay_clean() {
    assert_eq!(
        preserve_rich_markdown_line_breaks("one\ntwo\nthree\n\nnext\npara"),
        "one  \ntwo  \nthree\n\nnext  \npara"
    );
    assert_eq!(
        preserve_rich_markdown_line_breaks("a\n\n\nb\n   \nc"),
        "a\n\n\nb\n   \nc"
    );
    assert_eq!(
        preserve_rich_markdown_line_breaks("single line"),
        "single line"
    );
}

#[test]
fn crlf_is_normalized_without_stray_carriage_returns() {
    assert_eq!(
        preserve_rich_markdown_line_breaks("one\r\ntwo\r\n\r\nthree\r\n"),
        "one  \ntwo\n\nthree\n"
    );
}

#[test]
fn existing_hard_breaks_and_blockquotes_are_not_doubled() {
    assert_eq!(
        preserve_rich_markdown_line_breaks("a  \nb\\\nc\nd"),
        "a  \nb\\\nc  \nd"
    );
    assert_eq!(
        preserve_rich_markdown_line_breaks("> quoted one\n> quoted two\n>\n> after"),
        "> quoted one  \n> quoted two\n>\n> after"
    );
}

#[test]
fn fenced_code_blocks_are_untouched_inside_the_fence() {
    let input = "Run:\n```sh\necho one\necho two\n```\nDone\nok\n~~~~\nx\n~~~\ny\n~~~~\nend\nnow";
    assert_eq!(
        preserve_rich_markdown_line_breaks(input),
        "Run:\n```sh\necho one\necho two\n```\nDone  \nok\n~~~~\nx\n~~~\ny\n~~~~\nend  \nnow"
    );
    // A backtick line with an info string does not close an open fence.
    let input = "```\na\n```rust\nb\n```\nc\nd";
    assert_eq!(
        preserve_rich_markdown_line_breaks(input),
        "```\na\n```rust\nb\n```\nc  \nd"
    );
    // Indented code blocks keep their exact bytes too.
    let input = "intro\n\n    code one\n    code two\n\nafter\nmore";
    assert_eq!(
        preserve_rich_markdown_line_breaks(input),
        "intro\n\n    code one\n    code two\n\nafter  \nmore"
    );
}

#[test]
fn markdown_tables_are_untouched() {
    let input = "Results\n| a | b |\n| --- | --- |\n| 1 | 2 |\nTotal\nshown";
    assert_eq!(
        preserve_rich_markdown_line_breaks(input),
        "Results\n| a | b |\n| --- | --- |\n| 1 | 2 |\nTotal  \nshown"
    );
}

#[test]
fn inline_code_and_backslash_boundaries_keep_their_meaning() {
    for (input, expected) in [
        ("Use `one\ntwo`\nnext", "Use `one\ntwo`  \nnext"),
        ("Use ``one `\ntwo``\nnext", "Use ``one `\ntwo``  \nnext"),
        ("unmatched ` tick\nnext", "unmatched ` tick  \nnext"),
        ("escaped \\` tick\nnext", "escaped \\` tick  \nnext"),
        ("two\\\\\nnext", "two\\\\  \nnext"),
    ] {
        assert_eq!(preserve_rich_markdown_line_breaks(input), expected);
    }
    let input = "Use `$ONE\n$TWO`\nPay $5 with unmatched ` $6";
    let expected = "Use `$ONE\n$TWO`  \nPay \\$5 with unmatched ` \\$6";
    assert_eq!(
        send_message(100, input, None, None).body["rich_message"]["markdown"],
        expected
    );
    assert_eq!(
        edit_message_text(100, 42, input).body["rich_message"]["markdown"],
        expected
    );
}

#[test]
fn quoted_fences_and_indented_fence_text_do_not_leak_state() {
    let input = "> ```sh\n> echo $HOME\n> next\n> ```\nafter\nmore";
    assert_eq!(
        rich_markdown(input),
        "> ```sh\n> echo $HOME\n> next\n> ```\nafter  \nmore"
    );
    let input = "    ```\n    code\n\nafter\nmore";
    assert_eq!(
        preserve_rich_markdown_line_breaks(input),
        "    ```\n    code\n\nafter  \nmore"
    );
    // A shorter or different fence cannot close the block.
    assert_eq!(
        rich_markdown("````\n$x\n```\n$y\n~~~~\n$z\n````\n$a"),
        "````\n$x\n```\n$y\n~~~~\n$z\n````\n\\$a"
    );
}

#[test]
fn send_and_edit_keep_line_breaks_and_escape_dollars() {
    let input = "Paid\r\n• HK$100 `$PATH`\n• US$5\n```\necho $HOME\nx\n```";
    let expected = "Paid  \n• HK\\$100 `$PATH`  \n• US\\$5\n```\necho $HOME\nx\n```";
    let call = send_message(100, input, None, None);
    assert_eq!(call.body["rich_message"]["markdown"], expected);
    let edit = edit_message_text(100, 42, input);
    assert_eq!(edit.body["rich_message"]["markdown"], expected);
    // Dollars inside a tilde fence stay literal.
    assert_eq!(
        escape_rich_markdown_dollars("~~~\n$x\n~~~\n$y"),
        "~~~\n$x\n~~~\n\\$y"
    );
}

#[test]
fn send_message_and_edit_escape_currency_dollar_signs_outside_code_spans() {
    let input = "今日 S$123.45 同 US$170 已經入帳，HK$100 and HK$200 with `$PATH` and `$HOME`";
    let call = send_message(100, input, None, None);
    let md = call.body["rich_message"]["markdown"].as_str().unwrap();
    assert_eq!(
        md,
        "今日 S\\$123.45 同 US\\$170 已經入帳，HK\\$100 and HK\\$200 with `$PATH` and `$HOME`"
    );
    assert!(md.contains("123.45 同 US"));
    assert!(md.contains("100 and HK"));

    let edit_call = edit_message_text(100, 42, "Updated: HK$100 and HK$200");
    assert_eq!(
        edit_call.body["rich_message"]["markdown"].as_str().unwrap(),
        "Updated: HK\\$100 and HK\\$200"
    );
}
