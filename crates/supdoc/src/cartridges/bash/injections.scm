; Where links are read in Bash (03 R9, R14): every `#` comment, read as inline Markdown.
; Strings and heredocs are never regions, so `echo "[a](b.sh#c)"` is not a link.

((comment) @injection.content
  (#set! injection.language "markdown_inline"))
