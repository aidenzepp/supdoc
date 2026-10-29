; Where links are read in Rust (03 R9, R14): every comment, read as inline Markdown.
; String literals are never regions, so `let s = "[a](b.rs#c)";` is not a link.

((line_comment) @injection.content
  (#set! injection.language "markdown_inline"))

((block_comment) @injection.content
  (#set! injection.language "markdown_inline"))
