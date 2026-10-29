; Where links are read in TSX (03 R9, R14): every comment, read as inline Markdown. That includes
; `{/* ... */}` comments in JSX. String literals and JSX text are never regions.

((comment) @injection.content
  (#set! injection.language "markdown_inline"))
