; Bash symbols (03 R4). Functions are named by containment, so a function declared inside another
; is `outer.inner`. Variables are symbols only at the top level or behind `declare`, `readonly`,
; `export` and `local` at the top level: assignments inside a function body are locals, not API.

; Functions (`name() { … }` and `function name { … }` parse alike)

(function_definition name: (word) @name) @definition.function

; Top-level variables

(program (variable_assignment name: (variable_name) @name) @definition.constant)

(program
  (declaration_command
    (variable_assignment name: (variable_name) @name) @definition.constant))
