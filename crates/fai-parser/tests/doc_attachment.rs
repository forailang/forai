//! Doc-comment attachment rules: a comment block is a declaration's doc
//! comment only when it sits *directly* above it — a blank line breaks the
//! attachment and the block becomes a standalone comment (file-leading
//! blocks land in `Program.leading_comments`, in-body blocks become
//! `Statement::Comment`). Matches Go/Rust doc-comment conventions.

use fai_parser::ast::Statement;

fn fn_doc(program: &fai_parser::ast::Program, name: &str) -> Option<String> {
    program.statements.iter().find_map(|s| match s {
        Statement::Function(f) if f.name == name => Some(f.doc_comment.clone()),
        _ => None,
    })?
}

#[test]
fn adjacent_comment_attaches_as_doc() {
    let program = fai_parser::parse(
        "# Doubles x.\ndef double\n    @param x Int\n    @return Int\ndo\n    x * 2\nend\n",
    )
    .expect("parse");
    assert_eq!(fn_doc(&program, "double").as_deref(), Some("Doubles x."));
    assert!(program.leading_comments.is_empty());
}

#[test]
fn blank_line_detaches_leading_block_from_first_def() {
    // The harness-directive shape: file comments, blank line, then a def.
    let program = fai_parser::parse(
        "# expect: ok\n# stdout: 3\n\ndef main\n    @return Void\ndo\n    print(3)\nend\n",
    )
    .expect("parse");
    assert_eq!(fn_doc(&program, "main"), None, "directives must not be doc");
    assert_eq!(
        program.leading_comments,
        vec!["expect: ok".to_string(), "stdout: 3".to_string()]
    );
}

#[test]
fn blank_line_detaches_mid_file_block_into_comment_statement() {
    let src = "# Doc for a.\ndef a\n    @return Int\ndo\n    1\nend\n\n\
               # Stray note, not b's doc.\n\ndef b\n    @return Int\ndo\n    2\nend\n";
    let program = fai_parser::parse(src).expect("parse");
    assert!(fn_doc(&program, "a").is_some());
    assert_eq!(fn_doc(&program, "b"), None);
    let comment_blocks: Vec<&Vec<String>> = program
        .statements
        .iter()
        .filter_map(|s| match s {
            Statement::Comment(c) => Some(&c.lines),
            _ => None,
        })
        .collect();
    assert_eq!(comment_blocks.len(), 1, "stray block survives as a comment");
    assert_eq!(comment_blocks[0], &vec!["Stray note, not b's doc.".to_string()]);
}

#[test]
fn blank_line_detaches_inside_bodies_too() {
    let src = "def main\n    @return Void\ndo\n    # Setup note.\n\n    print(1)\nend\n";
    let program = fai_parser::parse(src).expect("parse");
    let Statement::Function(f) = &program.statements[0] else {
        panic!("expected function");
    };
    assert!(
        matches!(f.body.first(), Some(Statement::Comment(_))),
        "in-body detached block becomes a Comment statement"
    );
}

#[test]
fn multiline_adjacent_doc_still_attaches() {
    let program = fai_parser::parse(
        "# Line one.\n# Line two.\ndef f\n    @return Int\ndo\n    1\nend\n",
    )
    .expect("parse");
    assert_eq!(
        fn_doc(&program, "f").as_deref(),
        Some("Line one.\nLine two.")
    );
}
