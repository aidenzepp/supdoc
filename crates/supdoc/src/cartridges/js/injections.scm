; Where links are read in JavaScript (03 R9, R14): every comment, read as inline Markdown.
; String and template literals are never regions, so `const s = "[a](b.js#c)";` is not a link.

((comment) @injection.content
  (#set! injection.language "markdown_inline"))
