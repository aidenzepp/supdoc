; Where links are read in Go (03 R9, R14): every comment, read as inline Markdown.
; String literals are never regions, so `s := "[a](b.go#c)"` is not a link.

((comment) @injection.content
  (#set! injection.language "markdown_inline"))
